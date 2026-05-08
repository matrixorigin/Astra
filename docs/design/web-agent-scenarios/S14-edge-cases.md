# S14 — 极端边界：低端模型 8k 窗口 + workspace 切换 + 「继续」语义歧义

> Status: Draft v0.1
> Date: 2026-05-06
> Owner: astra runtime / web agent
> 关联设计文档: [../web-agent-session-state.md](../web-agent-session-state.md)
> 索引: [INDEX.md](INDEX.md)
> 被测 DAUT: **A1, A5, A14, A15**（全部一等被测；本场景是 14 个场景里面覆盖最广的收官压测）

## 1. 场景概览

### 1.1 用户画像

- **角色**：小姚，某中型 SaaS 公司产品经理，带头做「Chrome 插件 v2」原型（给客服团队用的快捷回复插件）。
- **身份**：企业用户，`user_id=u_yao`，所在公司 `org_acme` 开了**私有化合规**策略：生产 LLM 必须用公司本地部署的 `qwen-2.5-7b-instruct-int4`，`context_window=8192`，`max_output=500`。**云 LLM 被网关 block**。
- **使用习惯**：PM 思维跳、催得急、自然语言极其随意。同一句话里混三个代词，从不给限定词。
- **设备与环境（一天四次切）**：
  1. 早 08:30 家里 MacBook Pro，本地装了 astra edge bridge，`workspace_state=edge`，本地 repo `~/work/acme-helper-v2` 已克隆。
  2. 上午 10:15 上地铁切 iPhone 15，只有 web，无 edge，`workspace_state=cloud`（服务端沙箱 `sbx_yao_0506`）。
  3. 下午 14:00 到公司工位 PC（Win11 + Chrome），edge 重接，`workspace_state=hybrid`。
  4. 傍晚 18:30 客户现场 VPN 笔记本（公司发的 Chromebook + 客户 VPN），edge 路径被 VPN 策略 block，但**客户端不知道**（DNS 解析 ok、TCP 建链超时），`workspace_state` 名义上是 edge，实际 degraded。

### 1.2 业务背景

v2 原型**昨天**（2026-05-05）没做完，session `s_helper_v2_0506` 里挂着 plan，`session_todos` 有 12 条待办、3 条 `status='pending'`，其中一条是**昨天收工时** agent 主动写的 `suggested_next_action`（「抽奖特性合规性需 PM 确认」），在昨天 session 收尾时转成了 `agent_runs.status='waiting_for_user'`，过了一夜没人动。

### 1.3 一句话目标

> 让小姚在 **4 个环境、8k context window、自然语言极度歧义**的三重压力下，今天把 v2 原型推进到可交付，全程 agent 不能假装懂、不能卡死、不能在环境切换时用错工具。

### 1.4 关键数值指标

| 指标 | 数值 |
| --- | --- |
| 模型 | `qwen-2.5-7b-instruct-int4`，`context_window_tokens=8192`，`max_output_tokens=500` |
| 总轮数（LLM rounds） | 12 关键回合（另有约 8 轮内部 rerender/state sync 不计） |
| Tool call 数 | 27（多数是小文件编辑、运行 eslint、puppeteer 截图） |
| Workspace 切换次数 | 4（edge → cloud → hybrid → cloud-degraded） |
| 「继续」类歧义 utterance | 5 次，触发 A15 三级分级 |
| Manifest 被迫走「递进式加载」的回合 | 3（T5、T7、T10） |
| Session 持续时间 | 10h（含各设备碎片时间） |
| `context_manifests` 行数 | 12 |

### 1.5 三重压力速写

1. **8k 窗口极端预算**：朴素方案（把 anchor + plan + 最近 4 轮全塞）≥ 12k token，**根本装不下**。必须激进 drop 到 7500 以内，留 500 给模型思考输出。
2. **workspace 4 切**：每切一次 `tool_schemas` zone 可用工具集动态变，manifest 要重写 `included=false, reason='workspace_unavailable'`。
3. **「继续」三级识别**：用户说「继续」「就那个」「上周那个方案」，agent 要**不靠大模型**也能区分结构化续 / 规则续 / 真歧义。
4. **跨设备零缓存冷启 × 2**：iPhone 与 Chromebook 两台设备 IndexedDB 均空，两次都得从 MatrixOne 一次性重建 hot projection。

---

## 2. 被测设计假设

| DAUT | 解释 | 本场景如何压测 |
| --- | --- | --- |
| **A1** | MatrixOne 是 web session 唯一权威，浏览器只发 `session_id` + 增量 watermark，服务端重建 context | 4 台设备，两台零缓存冷启（iPhone、Chromebook），两台有 IndexedDB（MacBook、工位 PC）。每次切换都必须从 `session_state_items` + `session_todos` + `context_manifests` 三表重建。**`auth_sessions.device_fingerprint` 必须纳入 `state_revision` 计算**（见 §7 F6），否则 Chromebook 带着旧 workspace_state 打过来会污染服务端判断。 |
| **A5** | 每轮 token 预算按 zone 分配，总和 ≤ `context_window_tokens` | 8k 窗口是**非等比缩小**：不是把 200k 窗口的预算除以 25，而是要**改变 zone 优先级**。本场景要求：anchor 200 / plan_todo 400 / recent_tail 2000 / summary 500 / retrieved_facts 1000 / tool_previews 500 / system+tool_schemas 3400 = **7500**（留 192 safety + 500 output）。某些回合预算仍然吃紧，走「递进式加载」（先装 anchor+plan+tail，tool schemas 按需 hot-swap）。 |
| **A14** | Workspace 权威（cloud/edge/hybrid）持久化为 `workspace_state`，context 据此过滤可用工具 | 4 次切换，每次写 `session_state_items(category='workspace_state', item_key='authority', status='active')` 一行，`payload_json` 含 `{authority, sandbox_id?, edge_reachable?, tool_whitelist[]}`；`tool_schemas` zone 按 whitelist 动态裁剪（15→7→15→7_degraded）。**VPN block 场景下 edge 不可达但客户端侦测延迟** → 触发 §7 F1。 |
| **A15** | 下一步意图抽取优先级：structured event → rule → small model；低置信度进 `suggested_next_action` 而非自动执行 | 5 次歧义 utterance 走完三级分级：T1「继续」→ structured（直接命中 `waiting_for_user`）；T5「上周那个方案」→ rule 失败 + small model 给 3 候选；T9「那个不行换一个」→ 纯歧义 → `ask_user` with candidates。**严禁让 small model 猜然后自动执行**。 |

---

## 3. 会话时间线

每回合格式：**用户话 → workspace / 识别路径 → manifest 8k zone 真实 token → LLM 能/不能看到 → state 写入**。

### T01 — 08:30 家里 MacBook，"继续"（structured 命中）

- **用户**：「昨天那个 Chrome 插件 v2 继续。」
- **客户端**：MacBook 有 IndexedDB，watermark `state_revision=47, transcript_high_watermark=183, run_event_high_watermark=240`；POST `/chat/stream` 带 `session_id=s_helper_v2_0506`。
- **Workspace**：`workspace_state.authority='edge'`，edge bridge `bridge_mac_yao` 心跳 OK，工具 whitelist 15 项（含 `fs.read/write`、`shell.run`、`git.commit`、`puppeteer.screenshot`、`eslint.run` 等）。
- **A15 分级**：**第一级 structured**。`PostTurnExtractor` 查 `agent_runs WHERE session_id=? AND status='waiting_for_user'` → 命中 `run_id=r_v2_y5_0505_23:40`，`waiting_for='user_question:compliance_raffle'`。直接命中，**跳过 rule 与 small model**。
- **Manifest `ctx_s14_0001`**（`policy_version='budget_v1_8k'`, `reason='turn_start'`）：
  - `system_static=1700` / `tool_schemas=1700`（15 tools full schema）/ `session_anchor=195`（目标 + 合规白名单）/ `plan_todo=380`（active 3 条 + "(9 more)" 兜底）/ `recent_tail=1950`（昨天最后 3 轮）/ `summary=490`（昨天 L1b）/ `retrieved_facts=980`（抽奖合规的 2 条 decision）/ `tool_previews=0`（无近期 tool）/ `workspace=50`（一行注明 edge+bridge）。`total_estimated_tokens=7445`，`dropped_count=8`（更老的 turn、无关 suggestion）。
- **Agent 回应**：不写代码，而是直接问：「昨天你在『抽奖特性的合规讨论』上停了，`waiting_for_user:compliance_raffle`，要继续这个吗？或者你想切其他 pending todo（共 3 条）？」
- **LLM 不能看到**：更老的 47 轮 transcript、完整 tool outputs、其他 session 的 plan。
- **节省观察**：A15 L1 structured 命中只读 `agent_runs` 一行（索引 `idx_runs_session_waiting`）+ `session_state_items` pending 3 条，**不触发 small model**，本轮识别开销 ≈ 2ms DB + 0 LLM。

### T02 — 08:33 确认续 + 开干

- **用户**：「对就这个，你按昨天说的第三方合规 checklist 走，先读现在 manifest.json。」
- **Workspace**：仍 edge。Agent 调 `fs.read(~/work/acme-helper-v2/manifest.json)` → 走 `bridge_mac_yao` → 拿到 2 KB JSON；走 `session_tool_outputs`（preview 384 tokens）+ `session_artifacts(storage_backend='edge-local')`。
- **State 变化**：`agent_runs` 新起 `r_v2_y6_0506_08:33`, `status='running'`；昨天那条 `waiting_for_user` event 写 `user_resolved`；`session_state_items(category='workspace_state')` 不变；`session_state_item_events` 写一行 `compliance_raffle status=pending→resolved`。
- **Manifest `ctx_s14_0002`**（`reason='turn_start'`）：recent_tail 加入 T01 对话 +180 token；tool_previews 首次 ≠0 = 384；retrieved 压到 850（drop 最旧的 1 条 decision）；`total=7630`，略紧但通过（reserve 192 safety 还够）。`stable_prefix_hash` 与 T01 同（system+tool_schemas 未变 → prompt cache 命中，省 2GB/s LLM 带宽）。
- **LLM 看到**：manifest.json preview 前 384 token（剩余以 artifact_ref 链出）、T01 回合摘要。
- **LLM 不能看到**：完整 2KB JSON，若 agent 真要全文，必须调 `artifact.fetch(artifact_id)` 再消费；昨天 T4..T20 的被 compact 详情。

### T03 — 10:15 地铁切 iPhone：cloud 沙箱，tool_schemas 从 15 砍到 7

- **用户**：在地铁上打开 astra web，iPhone Safari。IndexedDB 空（首次此设备），客户端不带 `known_state_revision`。
- **服务端**：`GET /sessions/s_helper_v2_0506/state` 一次性下全量 hot projection；检测到 UA 变、`device_fingerprint=fp_iphone_yao_3f` 为此 session 首次出现，写 `auth_sessions` 新行。
- **A14 切 workspace**：
  - 写 `session_state_items(category='workspace_state', item_key='authority', status='active', payload_json='{"authority":"cloud","sandbox_id":"sbx_yao_0506","edge_reachable":false,"tool_whitelist":["sandbox.fs.read","sandbox.fs.write","sandbox.shell","web.search","artifact.fetch","plan.update","ask_user"]}')`
  - 写 `session_state_item_events` 一条 `workspace_changed from=edge to=cloud reason=device_switch`。
  - `state_revision` 推到 48。
- **Manifest `ctx_s14_0003`**：
  - `tool_schemas` zone 从 1700 → **700**（7 个精简 schema），节省 1000 token；
  - 释放的 1000 token 全给 `recent_tail`，让 tail 从 2000 → 2800（容纳更多上下文，弥补 iPhone 小屏用户打字少、依赖历史强的问题）；
  - `workspace` zone 注释变长到 120 token（写明「现在是 cloud 沙箱，本地 repo 不可读，必要时让用户 commit+push」）。
- **LLM 看到**：缩水的工具集、workspace 约束句。
- **LLM 不能看到**：edge 才有的 `fs.read` / `shell.run` / `git.commit` 这 8 个工具 schema（彻底从 prompt 里拿掉，不是标 disabled）。

### T04 — 10:18 Agent 撞墙 → manifest 加 workspace_constraint hint

- **用户**：「继续看下 background.js 那段消息监听。」
- **Agent 推理**：原本想调 `fs.read('background.js')`，但 workspace whitelist 里没这个。走 A14 降级路径：在 manifest 下一轮的 `session_anchor` 里嵌入 `workspace_constraint="cloud sandbox: no local repo; ask user to push or paste"`。
- **Agent 回应**：「我现在是 cloud 沙箱，读不到你 MacBook 上的 `background.js`。两个选项：① 你在 MacBook 上 `git push` 一下我从远端拉；② 把这段代码贴给我。」
- **用户**：「我没 commit，就 50 行你等一下我贴。」（5 秒后贴 47 行 JS）
- **Manifest `ctx_s14_0004`**：recent_tail +520 token（用户贴的代码），tool_previews 仍 0。`total=7510`，贴近红线。
- **State 写入**：`session_state_items(category='error_state', item_key='edge_unavailable_on_cloud', status='resolved_by_paste')`；贴的代码作为 artifact 写 `session_artifacts(storage_backend='matrixone_inline', preview_text_tokens=520)`。
- **LLM 不能看到**：MacBook 本地 47 行之外的其他文件、git history。

### T05 — 10:30 歧义触发：「上周那个方案不是说也能行吗」

- **用户**：「哎上周那个方案不是说也能行吗？」
- **A15 分级**：
  - **第一级 structured**：查 `agent_runs` 无 waiting、查 `suggested_next_action` 无 pending → 失败。
  - **第二级 rule**：扫 utterance 找限定词（"第 N 个"/"v2"/具体名词）→ 失败（"上周" + "那个" 指代不清）。
  - **第三级 small model**：7B 模型用 500 token budget 生成 2-3 个候选 → 返回 JSON：
    ```
    {"candidates":[
      {"id":"cand_a","summary":"上周四 IndexedDB 方案（存聊天历史本地）","source_turn":128,"confidence":0.62},
      {"id":"cand_b","summary":"上周三 chrome.storage.sync 方案","source_turn":102,"confidence":0.58},
      {"id":"cand_c","summary":"上周五讨论的 Service Worker 缓存方案","source_turn":141,"confidence":0.41}
    ]}
    ```
  - 最高 confidence 0.62，**低于 0.8 阈值**，**不自动执行**，走 ask_user。
- **Agent 回应**：「你说的是下面哪个？A: IndexedDB / B: chrome.storage.sync / C: Service Worker 缓存。」
- **Manifest `ctx_s14_0005`**：`retrieved_facts` 被压到 **1000 token 极限**，只能装 3 个 candidate summary + 指向 turn 128/102/141 的 `artifact_ref`。其它 retrieved candidate 全 drop (`reason='budget_exceeded'`)。`dropped_count=14`。
- **State**：`session_state_items(category='suggested_next_action', item_key='resolve_ambiguity_lastweek_plan', status='pending', expires_at=NOW()+2h)`。
- **LLM 不能看到**：其它 11 条被 drop 的 candidate 原文。

### T06 — 10:33 小姚选 A，8k 窗口开始吃紧

- **用户**：「A。」
- **A15**：这是对 T05 `suggested_next_action(resolve_ambiguity_lastweek_plan)` 的明确 resolution。写入 `session_state_item_events` 一条 `status=pending→accepted, chosen_candidate_id=cand_a, confidence_at_accept=1.0(user_explicit)`。
- **Agent**：读 turn 128 artifact，拿到 IndexedDB 方案细节；继续推进实现。此时因为仍在 cloud 沙箱，边界是「能拿到上周方案的 artifact 文本，但不能直接改 MacBook 上的代码」。
- **Manifest `ctx_s14_0006`** 压力点：retrieved_facts 被要求装 IndexedDB 方案原文（原本 820 token），但 zone 上限 1000 token，agent 用 `PromptRenderer` 的 `render_mode='compressed_summary'` 压到 680 token，保留 `source_hash` 供后续 artifact 回补。被 drop 的方案 B/C 在 manifest item 里记 `included=false, reason='user_rejected_candidate'`。
- `total_estimated_tokens=7488`；`stable_prefix_hash` 未变（cloud 的 7 个 schema + system prompt 不变），prompt cache 继续命中。

### T07 — 10:45 plan_todo 爆：12 条 todo 只能渲 3 条

- **用户**：「把 plan 给我看看。」
- **问题**：`session_todos` 有 12 条，平均每条 80 token，完整列出需 960 token，但 `plan_todo` zone 上限 400 token。
- **策略**：渲染 `status='active'` 的 3 条（~240 token）+ 一行「(9 more todos — query for details)」兜底（~40 token），剩 120 token 给 plan header。其它 9 条 drop `reason='budget_plan_todo_overflow'`。
- **Agent 回应**：「现在 active 3 条：①合规 checklist ②IndexedDB 实现 ③icon 对齐；还有 9 条 pending/done，要看请说『plan 全部』。」
- **Manifest `ctx_s14_0007`**：展示 A5 在小窗口下的「显式降级 + 可追回」 —— 被 drop 的 9 条在 `context_manifest_items` 里留了 `included=false, source_id=todo_xxx, reason='budget_plan_todo_overflow'`。
- **关键点**：这不是「遗忘」而是「按需展开」。若用户后续说「plan 全部」，context builder 下一轮临时把 `plan_todo` 提到 800 + 砍 `recent_tail` 到 1600（临时借预算，单轮有效）。

### T08 — 14:00 到公司切工位 PC：hybrid 恢复

- **用户**：到公司，开 Chrome 登 astra，IndexedDB 里存着上周几次的 `state_revision=41`（过期了 7 个）。
- **客户端 → 服务端**：`GET /sessions/.../state?known_state_revision=41` → 服务端算 delta 下发 42..48，外加 turn 183..190 transcript。
- **A14 切 workspace**：工位 PC 装了 edge bridge `bridge_pc_yao`；但此 PC 只能访问内网 repo 镜像而非用户个人 MacBook。agent 检测到 `edge_reachable=true` 但 `workspace_fingerprint` 与 T01 的 MacBook edge 不同。
  - 写 `workspace_state.payload_json='{"authority":"hybrid","sandbox_id":"sbx_yao_0506","edge_reachable":true,"edge_id":"bridge_pc_yao","tool_whitelist":[15 tools]}'`
  - `state_revision` 推到 49。
- **Manifest `ctx_s14_0008`**：`tool_schemas` 重新扩到 1700；但因为 tail 已经累积到 2100 token，要挤回 2000：drop T03 那次 workspace_constraint 的 520 token 用户贴码（`reason='older_turn_has_artifact_ref'`，可通过 artifact_id 回查）。
- **Agent 回应**：「切到公司 PC 了，edge 回来了。本地 repo 这台机器是内网镜像，你 MacBook 上改的还没 push，要先同步吗？」

### T09 — 14:20 纯歧义：「那个不行，换一个」

- **用户**：「这个方案不行换一个。」
- **A15 分级**：
  - **第一级 structured**：当前 `waiting_for_user` 无 → 失败。
  - **第二级 rule**：「这个方案」没有指向 turn / plan / todo，rule 失败。
  - **第三级 small model**：生成 2 候选：`{T5 方案 A: IndexedDB, T8 方案 B: 内网同步策略}`，两者 confidence 分别 0.55 / 0.48，均 < 0.8。
- **Agent 回应**：「你指的是 T5 挑的 IndexedDB 方案，还是 T8 我们讨论的『先 push 再同步』思路？」
- **写入**：`suggested_next_action` pending，`expires_at=NOW()+30min`（比 T05 短，因为会话活跃）。
- **Manifest `ctx_s14_0009`**：retrieved_facts 装两个候选摘要（共 ~700 token），未满，剩 300 token 匀给 recent_tail。`total=7320`。

### T10 — 15:40 用户选 IndexedDB、继续开干，期间 plan 扩到 15 条

- **用户**：「IndexedDB。你接着写 storage 层和迁移逻辑。顺带把错误处理加上。」
- **Agent**：调用 edge `fs.write` 写 `src/storage/indexed_db.ts`，调 `eslint.run` 校验。
- **8k 吃紧升级**：tool_previews 这一轮 ≠ 0（eslint 输出 320 token），recent_tail 3 条对话加 artifact 描述 ~2400 token，退到 2000 要 drop 400。走 `render_mode='compressed_summary'` 对倒数第 3 轮做压缩。
- **递进式加载**启动：agent 发现单轮要同时看「方案 A 原文 + ESLint 输出 + background.js 代码 + storage 层 skeleton」，总超 1200 token 而 retrieved_facts + tool_previews 合计只给 1500。策略：先以 summary 进 manifest，真正要消费某块时按需 `artifact.fetch` 拉（第二轮才加入）。
- **Manifest `ctx_s14_0010`**：`reason='turn_with_progressive_loading'`（policy 里新增的一种 reason），`dropped_count=21`，但每个 drop 都有 `source_id` + `artifact_ref`，下一轮可回补。

### T11 — 18:30 客户现场 VPN Chromebook：edge 被 block，agent 侦测盲区

- **用户**：Chromebook 登 astra，IndexedDB 空，客户端冷启。VPN 让 DNS 解析 `bridge-yao.local` 成功（被 VPN 的假 split-tunnel DNS 响应）但 TCP 443 建链会 hang 60 秒超时。
- **客户端上报**：`workspace_hint="edge"`（因为 DNS 解析成功，客户端错判 edge 可达）。服务端写入 `workspace_state` 乐观标 `edge_reachable=true`。
- **Agent 第一次调 `fs.read`**：`bridge_pc_yao` 通过 relay 调不通 Chromebook 上的 bridge（因为 Chromebook 根本没装 bridge）。服务端 `edge_tool_call` 进 `waiting_for_edge`，60 秒超时。
- **A14 降级路径触发**：服务端侧超时后，写：
  - `session_state_items(category='workspace_state', item_key='authority', status='degraded', payload_json='{"authority":"cloud","reason":"edge_unreachable_after_probe","prev_authority":"edge"}')`；
  - `session_state_items(category='error_state', item_key='edge_probe_timeout', status='active')`；
  - `agent_run_events` 写 `workspace_downgraded` 事件。
- **Manifest `ctx_s14_0011`**：`tool_schemas` 立刻砍回 cloud 7 项；`workspace` zone 120 token 说清「已从 edge 降级为 cloud，原因：edge_probe_timeout」。`state_revision` 推到 52。
- **Agent 回应**：「你现在这台 Chromebook 连不到你 MacBook 的 edge，我退成 cloud 了。要继续就告诉我这边的最新代码，或者我直接用 T10 写过的 skeleton 往下推。」
- **用户体验成本**：60 秒首次工具调用 hang 是**本场景最糟糕的单点体验**（见 F1），修掉它是设计反馈里优先级最高的一条。

### T12 — 19:45 交付 + 小姚手动 commit

- **用户**：「就 T10 那版 skeleton，你把 README 和 changelog 补一下，我回去 MacBook 合。」
- **Agent**：在 cloud 沙箱生成 `README_v2.md` + `CHANGELOG_v2.md`，写 `session_artifacts(storage_backend='matrixone_inline')` 两份；**Agent 不擅自动 MacBook repo**（workspace degraded 语义严格遵守：只有 `authority=edge` 且 `edge_reachable=true` 才能写本地 repo）。
- **最后 Manifest `ctx_s14_0012`**：`reason='turn_finalize'`, `total_estimated_tokens=7190`（因为 tool_previews 降下来了）；为了让下次开 session 能快速拿到「今天进度」，写了一条 `session_state_items(category='summary', item_key='day_progress_0506', status='active', token_estimate=240)`。
- **Session 收尾**：`agent_runs.status='completed'`；`session_state_items(category='suggested_next_action', item_key='user_commit_merge', status='pending', expires_at=NOW()+24h, payload_json='{"action":"git_commit+push+pr","target_device":"MacBook","files":["README_v2.md","CHANGELOG_v2.md","src/storage/indexed_db.ts"]}')`，小姚回 MacBook 后首次打开 astra 会看到「还有一步要你亲自来」的提示卡（A15 低置信度走 `suggested_next_action` 而非自动执行的典型）。
- **审计闭环**：整场 session 写了 12 条 `context_manifests`、4 条 `workspace_state` 变更、5 条歧义识别 event、0 条 manifest 被客户端污染（A1 保持）。

---

## 4. 上下文压力点

| 压力点 | 本场景特有 | 设计必须扛住 |
| --- | --- | --- |
| **8k 硬顶** | 6/12 回合 `total_estimated_tokens ∈ [7300, 7700]`，离红线 <5% | Budget Policy v1 必须在 `policy_version='budget_v1_8k'` 下给出不同于 200k 窗口的 zone 配比；压缩不是等比 |
| **4 次 workspace 切换** | 每次 `tool_schemas` zone 体量震荡（700↔1700），tail 要顺势 rebalance | `ContextAssembler` 必须把 `workspace_state` 作为 first-class 输入，不能事后 patch |
| **歧义三级** | 5 次 utterance 命中不同级，严禁 small model 猜对就自动跑 | A15 的 `suggested_next_action.status=pending` + `expires_at` + `confidence` 必须成 first-class 字段 |
| **递进式加载** | T10 一轮要 4 种内容，单轮塞不下 | `context_manifests.reason='turn_with_progressive_loading'` 作为合法取值；被 drop 的要保留 `artifact_ref` |
| **Edge 通透性盲区** | T11 DNS 成功 + TCP 超时，客户端误判 edge ok | `workspace_state` 需要 `reachability_probe` 字段（见 §8.1 建议） |
| **零缓存冷启 × 2** | iPhone、Chromebook 两台无 IndexedDB，不能污染服务端 | A1 契约：请求仅 `session_id` + 无 `known_state_revision` → 服务端全量下发；`device_fingerprint` 必须纳入 `state_revision` 计算 |

---

## 5. 期望的 state projection 演化

列出 T01/T03/T08/T11 四个关键节点快照，展示 workspace / tool_schemas zone / 8k zone 分配 / 歧义识别决策。

### 5.1 T01 08:30（MacBook, edge）

```text
session_state_items:
  category=workspace_state  item_key=authority  status=active
    payload={authority:edge, edge_id:bridge_mac_yao, tool_whitelist:[15 tools]}
  category=suggested_next_action item_key=compliance_raffle status=pending expires_at=T01+24h
agent_runs: r_v2_y5_0505_23:40 status=waiting_for_user waiting_for=user_question:compliance_raffle
context_manifests.ctx_s14_0001:
  context_window=8192, total=7445, dropped=8, policy=budget_v1_8k
  zones: system=1700, tool_schemas=1700(15), anchor=195, plan_todo=380, tail=1950,
         summary=490, retrieved=980, tool_previews=0, workspace=50
```

### 5.2 T03 10:15（iPhone, cloud）

```text
session_state_items (delta):
  workspace_state.authority=active 变更 from=edge to=cloud, sandbox_id=sbx_yao_0506
  state_revision: 47 → 48; device_fingerprint 新增 fp_iphone_yao_3f
context_manifests.ctx_s14_0003:
  zones: system=1700, tool_schemas=700(7), anchor=195, plan_todo=380, tail=2800,
         summary=490, retrieved=980, tool_previews=0, workspace=120
  total=7363; tool_schemas 释放的 1000 token 全部转给 tail（iPhone 小屏需强历史）
dropped_items.reason='workspace_unavailable' 命中 8 个 edge-only tool schema
```

### 5.3 T08 14:00（工位 PC, hybrid）

```text
session_state_items (delta):
  workspace_state.authority=active 变更 to=hybrid, edge_id=bridge_pc_yao
  error_state.edge_unavailable_on_cloud status=resolved（T04 遗留）
context_manifests.ctx_s14_0008:
  zones: system=1700, tool_schemas=1700(15), anchor=195, plan_todo=380, tail=2000,
         summary=490, retrieved=980, tool_previews=320, workspace=110
  total=7575; drop 了 T04 用户贴码 520 token, reason='older_turn_has_artifact_ref'
```

### 5.4 T11 18:30（Chromebook VPN, edge→cloud degraded）

```text
session_state_items (delta):
  workspace_state.authority from=edge(optimistic) to=cloud, status=degraded
    payload.reason=edge_unreachable_after_probe, prev_authority=edge
  error_state.edge_probe_timeout status=active
agent_run_events: workspace_downgraded event_idx=412
context_manifests.ctx_s14_0011:
  zones: system=1700, tool_schemas=700(7), anchor=195, plan_todo=380, tail=2500,
         summary=490, retrieved=980, tool_previews=0, workspace=120
  total=7065（比前轮更省：Chromebook 冷启没有 tool preview 需要复用）
  dropped_count=11, reasons=[workspace_unavailable:8, budget_plan_todo_overflow:9 已折叠, older_turn:…]
```

---

## 6. Token 节省估算

> 本场景核心不是对比「不 drop vs drop」，而是证明 **8k 窗口下朴素方案根本不可行**，且必须用与大窗口不同的策略。

### 6.1 朴素方案（把 anchor + plan 完整 + 最近 4 轮 + 完整 tool output + 所有 retrieved facts 全塞）

- session_anchor：完整目标 + 合规白名单 + 上周决策 ≈ **1100 token**
- plan（完整 12 条 todo） ≈ **960 token**
- 最近 4 轮原文（含贴码 + eslint 输出 + diff） ≈ **4800 token**
- 完整 tool output（eslint raw + file read full） ≈ **2200 token**
- retrieved（三周历史 decision/file/error） ≈ **3500 token**
- system+tool_schemas（15 个完整 schema + safety） ≈ **3400 token**
- **合计 ≈ 15960 token**，超 8k 的 **1.95 倍** → **直接 prompt 不进**（模型调用会 400）。

### 6.2 本设计（budget_v1_8k policy）

| Zone | 200k 窗口典型配比 | 8k 窗口配比 | 变化 |
| --- | --- | --- | --- |
| system_static | 2000 | **1700** | 安全契约文本裁短、去示例 |
| tool_schemas | 8000 | **700–1700**（动态） | 仅全量 tool 的 essential schema，param doc 删 |
| session_anchor | 600 | **200** | 去掉历史白名单枚举，改 ref |
| plan_todo | 1000 | **400** | 只 active 3 条 + 尾注；其余靠 `ask` 扩展 |
| recent_tail | 4000 | **2000（可动到 2800）** | 最关键压缩区，借 tool_schemas 空出的空间动态调 |
| summary | 2000 | **500** | 仅一段浓缩 L1b |
| retrieved_facts | 6000 | **1000** | Top-2~3，`render_mode=compressed_summary` |
| tool_previews | 3000 | **0–500** | 首选 artifact_ref；只在本轮刚调完工具时才进 |
| workspace | 200 | **50–120** | 一行字 |
| **合计** | ~26800 | **7500**（留 192 safety + 500 output） | **节省 72%** |

### 6.3 额外成本（诚实交代）

| 成本项 | 量级 |
| --- | --- |
| 递进式加载的额外 `artifact.fetch` | 3 回合 × 平均 1.5 次额外 tool = 4.5 次 |
| 小模型歧义识别 | 2 次 × 平均 500 prompt + 200 completion = ~1400 token（但本地 7B 免费） |
| 渲染 compressed_summary 的开销 | 每次约 20ms，可接受 |

### 6.4 一句话结论

> 8k 窗口不是「200k 窗口砍 1/25」，而是要**改变 zone 优先级**（tool_schemas 可跌到 700，tail 必须保底 2000）+ **承认单轮装不下就走递进式加载**。Budget Policy v1 要在设计文档里补一张 **`budget_v1_8k` 明确模板表**，否则实现各做各的。

---

## 7. 潜在失败模式

按严重度排序。

### F1 — Edge 可达性只靠 DNS，VPN block 变通透盲区

**触发**：T11 Chromebook VPN 让 DNS 撞 split-tunnel 假响应，TCP 真建链 hang；客户端上报 `edge_reachable=true`，服务端乐观信任。
**信号**：第一次 `fs.read` 调用 `waiting_for_edge` 卡 60 秒；用户体验「按了但没反应」。
**根因**：`workspace_state` 只存 authority，**没有 reachability probe**。
**防线**：设计加 `reachability_probe={last_ok_at, last_fail_at, probe_method, rtt_ms}`；cloud relay 接到 edge 调用先快速 HEAD 探活（200ms 超时），不通直接走降级。

### F2 — Workspace 切换期间 context cache 未失效

**触发**：T03 切 cloud 时，context builder 上一轮 cache 了 `fs.read` 工具 schema；若缓存 key 不含 `workspace_fingerprint`，会把 edge tool schema 带进 cloud manifest。
**信号**：cloud 回合 `tool_schemas` 里出现 `fs.read`，agent 调了会报 unknown tool。
**根因**：PromptRenderer / ToolSchemaSelector cache key 缺 workspace 维度。
**防线**：cache key 拼 `(session_id, workspace_fingerprint, policy_version)` 三元组；workspace_changed 事件立刻 invalidate。

### F3 — 小窗口 recent_tail 砍狠，agent 失去短期记忆

**触发**：T07 被要求只留 2000 token tail；若 compaction 实现不到位把前一轮贴码全 drop 而无 artifact_ref，下一轮 agent 答「你刚刚贴的代码是什么来着」。
**信号**：用户问「刚才那段代码」，agent 反问。
**根因**：tail 压缩时没遵循「保留 L1a 结构化事实 + artifact_ref」。
**防线**：tail 压缩时把代码块转 `session_artifacts` + `preview_text`（首 200 token），不直接删；manifest 写 `source_id=artifact_xxx`。

### F4 — 「继续」候选太多用户烦，ask_user 疲劳

**触发**：T05 给 3 候选、T09 又给 2 候选、若 T10 再歧义又一轮 —— 一小时内 3 次让小姚选，她会骂。
**信号**：用户回「你自己看着办」；心累放弃。
**根因**：confidence 阈值太紧、候选分级太粗。
**防线**：引入阈值分层 —— confidence > 0.8 自动执行并在回答里声明依据、0.5–0.8 列候选、< 0.5 直接承认「没看懂你意思」；三次内同一 session 触发 ask_user 时降阈值一次（用户显然更急）。

### F5 — tool_schemas 动态裁剪导致 agent 误以为工具不存在

**触发**：T03 裁掉 `git.commit` schema 后，agent 下轮要提议「要不要 commit」时**根据 prompt 里没有这个工具，以为不存在**，于是根本不提，而不是提示「你得回 MacBook 自己 commit」。
**信号**：用户主动问「要不要 commit」时 agent 才反应；或 agent 说「我没有 commit 工具」把它当物理事实。
**根因**：LLM 看不见被裁掉的 schema，但人类知识里 git 是常见操作 —— 要区分「本 context 不可见」和「根本不存在」。
**防线**：workspace zone 注释里写明「cloud 沙箱不装 git 工具，用户要自行 commit」，让 agent 知道这是**环境限制**而非**能力缺失**。

### F6 — `state_revision` 不含 device_fingerprint，跨设备回退

**触发**：T11 Chromebook 带着自己乐观生成的 `state_revision=52` 打回去，若此前 MacBook 已推到 53，服务端可能回退（取决于实现细节）。
**信号**：T12 小姚回 MacBook 看到之前的修改消失。
**根因**：`state_revision` 只是单调计数器，不记载「谁推进的」。
**防线**：`state_revision` hash 纳入 `(monotonic_id, device_fingerprint, timestamp)`；服务端收到请求 if-none-match 时比 monotonic_id；UI 也给出「最近由设备 X 推进到 revision Y」的提示。

### F6b（次级）— `suggested_next_action` 过期未清

**触发**：T05 写的 `expires_at=T05+2h`，T07 进来时还没到期但已无关。
**信号**：context builder 把过期 suggestion 仍塞进 manifest，占 retrieved_facts 预算。
**防线**：context 入选前 `WHERE expires_at > NOW() OR status='accepted'` 过滤；后台 sweeper 把 `status='pending' AND expires_at < NOW()` 的标 `expired`。

---

## 8. 对设计的反馈

### 8.1 设计文档需补强的章节

1. **§"Budget Policy v1"** 目前只列大窗口 zone 典型值，**缺小窗口专用模板**。本场景证明 `budget_v1_8k` 与 `budget_v1_200k` 不是等比关系。建议在表格后加一段：
   > For small-window models (≤ 16k), use `budget_v1_8k` template: anchor 200 / plan_todo 400 / recent_tail 2000（可临时扩到 2800）/ summary 500 / retrieved 1000 / tool_previews 500 / system+tool_schemas 3400. Reserve 500 output + ~200 safety.
2. **§"Cloud Workspace vs Edge Workspace"** 的 `workspace_state` schema **缺 `reachability_probe` 字段**（F1）。建议 payload_json 加：
   ```
   reachability_probe: {last_ok_at, last_fail_at, probe_method, rtt_ms}
   ```
   并要求 cloud relay 在转发 edge tool call 前先 200ms HEAD 探活。
3. **§"Next-Action Extraction Cost"** 已提到 structured / rule / small-model 三级，**但未规定 confidence 阈值**。建议明确：
   - `≥0.8` 自动执行 + 回答里标注依据；
   - `0.5–0.8` 列候选 ask_user；
   - `<0.5` 直接「没看懂」；
   - 同 session 1h 内连续 3 次 ask_user 降一档阈值（F4）。
4. **§"Web Client Cache"** 未规定 `state_revision` 组成。建议：`state_revision` 等效 hash 纳入 `device_fingerprint`（F6），服务端实现 if-none-match 时比 `monotonic_id` 而非 hash 整体，保证单调性的同时让回退可检测。

### 8.2 Open Questions

- Q1：`budget_v1_8k` 的 token 估算方法是否和 `budget_v1_200k` 共用？7B 的 tokenizer 和 GPT-4o 不同，直接套字节估算会飘 15%。→ 建议 manifest 记录 `tokenizer_id`。
- Q2：`suggested_next_action` 的 `expires_at` 默认值该多长？本场景用 30min–24h 不等，需要策略文档。
- Q3：workspace 切换时是否要重算 `context_manifest` 还是等下个 turn 自然重建？本场景采取了后者（T03 下个 LLM turn 才 rebuild），但 UI 的「Context」tab 会显示过期信息 1 秒。

### 8.3 设计能力评估（本场景维度）

| DAUT | 当前设计评级 | 备注 |
| --- | --- | --- |
| A1 | ◐ | state_revision 三元 watermark 清晰，但未纳入 device_fingerprint；跨设备回退可能 |
| A5 | ⚠️ | Budget Policy v1 只给大窗口示例，小窗口模板缺失；递进式加载 reason 未枚举 |
| A14 | ⚠️ | workspace_state 字段够用，但缺 reachability_probe；降级决策路径未文档化 |
| A15 | ◐ | 三级分级已写，但 confidence 阈值未定；ask_user 疲劳策略缺 |

---

## 附录 A：Zone 预算对比表（200k vs 8k）

见 §6.2 表格，核心结论：**8k 窗口下 tool_schemas 与 retrieved 必须重度压缩，tail 必须保底**。

## 附录 B：紧凑接口调用序列

```text
T01 08:30  POST /chat/stream { session_id, watermarks{rev=47,tx=183,re=240} }
           → resume hit waiting_for_user, ask_user(compliance_raffle?)
T02 08:33  POST /chat/runs/.../input { approval_response }
           → r_v2_y6_0506_08:33 started, fs.read(manifest.json) via edge bridge
T03 10:15  GET /sessions/s_helper_v2_0506/state           （iPhone 冷启，无 watermark）
           → workspace_changed edge→cloud，tool_schemas 15→7，state_revision 48
T04 10:18  POST /chat/stream "看 background.js"
           → 无 edge tool, manifest 加 workspace_constraint, ask user 贴码
T05 10:30  POST /chat/stream "上周那个方案"
           → A15 L3 small-model, 3 candidates, confidence 0.62, ask_user
T06 10:33  POST /chat/stream "A"
           → suggested_next_action accepted, render compressed summary
T07 10:45  POST /chat/stream "plan"
           → plan_todo zone overflow, 3 active + "(9 more)" 兜底
T08 14:00  GET /sessions/.../state?known_state_revision=41（工位 PC 有 stale cache）
           → delta 42..48 + workspace edge re-attached, state_revision 49
T09 14:20  POST /chat/stream "这个方案不行"
           → A15 L3 2 candidates, both <0.8, ask_user
T10 15:40  POST /chat/stream "IndexedDB / storage / errors"
           → edge fs.write + eslint.run, progressive loading, dropped 21
T11 18:30  GET /sessions/.../state                         （Chromebook 冷启）
           → optimistic edge, first fs.read timeout 60s
           → workspace_downgraded edge→cloud, state_revision 52
T12 19:45  POST /chat/stream "补 README + changelog, 我回去合"
           → cloud inline artifacts, suggested_next_action(user_commit_merge)
```

## 附录 C：本场景不覆盖什么

- 大模型长窗口优化（→ S01, S02）
- 多 agent delegation（→ S09, S10）
- 长审批等待（→ S07）
- 单设备断线重连（→ S04）

本场景的唯一假设压力是：**小模型 + 小窗口 + 用户说话极随意 + 一天四台设备，agent 不能崩、不能假装、不能在环境切换时用错工具。**

这也是整个 14 场景系列的收官 —— 如果 A1/A5/A14/A15 在这种条件下全绿，则可以相信设计在真实中小公司落地时能扛住「合规要求 + 小模型 + 移动办公」三件事叠加的日常。
