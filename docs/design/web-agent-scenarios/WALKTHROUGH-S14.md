# WALKTHROUGH-S14 — 8k 窗口 × 4 设备切换 × 歧义「继续」端到端推演

> Scope: 拿 `web-agent-session-state.md` v0.2（G1–G19 全 resolved）作为唯一权威，把 S14 的 T1–T12 实打实走一遍；
> 视角: 一个要写 `ContextAssembler` / `NextActionExtractor` / `WorkspaceProbe` 的工程师；
> 判据: 机制是否可达 / 体验是否顺畅 / 别扭点在哪 / 小窗口 token 是否装得下。

---

## §1 场景标识与 v0.2 映射

**一句话**：PM 小姚在 `qwen-2.5-7b-instruct-int4` 私有化 8k 窗口下，一天从家 MacBook edge 切到地铁 iPhone cloud、再到公司 PC hybrid、最后到客户现场 Chromebook VPN（edge 被 block），全程用"继续 / 那个 / 上周方案 / 不行换一个"这类极随意语言推进 Chrome 插件 v2 开发。

**被测 DAUT**：A1（服务端权威 + watermark）、A5（分 zone 预算）、A14（workspace authority）、A15（next-action 分级）。

**本场景最关键的 v0.2 字段/契约**：
- `context_manifests.budget_template_id='budget_v1_8k'` + `tokenizer_id` @L1449–1450；小窗口模板定义 @L2410–2438（G10）
- `workspace_state.payload_json.reachability_probe` + online/offline/reconnecting/detached 四态 @L2669–2700（G11）
- `suggested_next_action` 三档阈值 + ask-user 疲劳降档 @L665–696（G12）
- `state_revision={monotonic_id, revision_hash, device_fingerprint}` + `session_device_leases` @L438–498（G13）
- `delegation_state` zone 预算公式 @L2442–2471（G18）
- Plan Tree Rendering Policy 祖先链 + 当前 section + 兄弟标题 @L2249–2295（G5）—— T7 plan 12 条被迫 render

---

## §2 运行前提

**Schema 初值**：
- `agent_sessions.session_id=s_helper_v2_0506`，`project_id=proj_acme_chrome_helper`，上一 run `r_v2_y5_0505_23:40` 状态 `waiting_for_user`，`waiting_for='user_question:compliance_raffle'`，昨晚 23:40 存档；
- `session_todos` 12 条（3 active / 6 pending / 3 done），`depth` 范围 0–2，均无 archived；
- `session_state_items`：`workspace_state.authority=edge`（昨晚 MacBook 关机前未清）、`suggested_next_action.compliance_raffle` pending、L1b `summary.day_0505` active；
- `context_manifests` 昨天收盘最后一条 `ctx_0505_final` 存档，`policy_version='budget_v1_8k'`；
- `session_device_leases` 中 MacBook 的 `fp_mac_yao` 已 `trust_level='trusted'`，iPhone/Chromebook 未注册；工位 PC 上周登记过 `trust_level='trusted'` 但过期待 reattach。

**Workspace 序列**：edge(MacBook) → cloud(iPhone, `sbx_yao_0506`) → hybrid(工位 PC, `bridge_pc_yao`) → edge→cloud degraded(Chromebook VPN)。

**Budget 模板**：`budget_v1_8k`（L2418–2425）：anchor 200 / plan_todo 400 / recent_tail 2000（可扩 2800）/ summary 500 / retrieved 1000 / tool_previews 500 / system+tool_schemas 3400 / 输出 500 + 安全 200。**Vector retrieval 默认关闭**（L2433–2435），除非 structured + FTS 全 miss 且 retrieved 还剩 1000 token。

**tokenizer_id**：`qwen2.5-int4-bpe-v1`，和 GPT-4o tokenizer 漂移 ~12%，所以 `context_manifests` 每条都要写 `tokenizer_id`。

**本场景用的 budget_v1_8k 完整模板**（v0.2 L2418–2425 实录）：

```text
context_window_tokens      = 8192
reserved_output            =  500   (L2425)
safety_buffer              =  200   (L2426)
-----------------------------------------
content_available          = 7492

zone cap:
  system_static            = (部分 of 3400)  典型 1700
  tool_schemas             = (部分 of 3400)  边缘 15 tools 时 1700, cloud 7 tools 时 700
  session_anchor           =  200
  plan_todo                =  400
  recent_tail              = 2000   floor 1600, 临时扩 2800 (L2431)
  summary                  =  500
  retrieved_facts          = 1000   vector 默认关 (L2433)
  tool_previews            =  500   0 当本轮无新 tool
  workspace                =   50   (未在 L2418 列出, 实现里常 50-120)
  delegation_state         = 1500   (G18, S14 本身 0)
```

S14 无子 agent，所以 delegation zone = 0；剩余 7492 token 全部用于上面 9 个 zone。

---

## §3 端到端运行轨迹

> 每回合三列：**User / Server behavior（v0.2 字段/表） / User sees（SSE、UI、zone 实际 token）**。
> 所有 token 数用 `qwen2.5-int4-bpe-v1` 估算。

### T01 — 08:30 家里 MacBook：「昨天那个 Chrome 插件 v2 继续。」

- **User**：Command+K 打开 astra，直接敲「昨天那个 Chrome 插件 v2 继续。」回车。
- **Server**：
  - 浏览器 POST `/chat/stream { session_id=s_helper_v2_0506, known_state_revision={monotonic_id:47, hash:sha256:…, device_fingerprint:fp_mac_yao} }`；
  - G13 路径：服务端比 `monotonic_id=47` 在 retention 窗内 → 走 **delta** 不重置；
  - `PostTurnExtractor` 执行 G12 L1 **structured 命中**：`SELECT run_id FROM agent_runs WHERE session_id=? AND status='waiting_for_user'` 拿到 `r_v2_y5_0505_23:40`；命中后 `confidence=1.0(structured)` → 但此时 *是否自动执行* 需要二次判定：用户话中含"继续"动词但未明确确认 → 按 G12 L669 "structured_event hit → accepted, confidence=1.0"，直接认定为 resolve 该 waiting；
  - `ContextAssembler` 开 manifest `ctx_s14_0001`，`budget_template_id='budget_v1_8k'`，`reason='approval_resume'`（G1 枚举里对应昨晚 waiting_for_user 复活）。
- **User sees**：SSE 先推 `run_started(run_id=r_v2_y6_0506_08:30)`，立刻跟一条 `assistant_message`：「昨天你停在『抽奖特性合规讨论』，`waiting_for_user:compliance_raffle`。要继续这个？还是切到其他 pending todo（共 3 条）？」
- **Manifest zones**（total=7445）：system_static=1700 / tool_schemas=1700(15 edge tools) / anchor=195 / plan_todo=380 / recent_tail=1950 / summary=490 / retrieved=980 / tool_previews=0 / workspace=50 / delegation=0。`dropped_count=8`（更老 47 轮 + 过期 suggestion）。

### T02 — 08:33 用户确认 + agent 读 manifest.json

- **User**：「对就这个，你按昨天说的第三方合规 checklist 走，先读现在 manifest.json。」
- **Server**：
  - `agent_runs.r_v2_y6_0506_08:30` 状态保持 running；
  - `session_state_item_events` 写一条 `mutation=update, category=suggested_next_action, item_key=compliance_raffle, status=pending→accepted, apply_suggestion.ref=sna_...`（G14 枚举）；
  - 调 `fs.read(~/work/acme-helper-v2/manifest.json)` 经 `bridge_mac_yao`；
  - `session_tool_outputs` 写一行，`preview_template='fs.read_v1'`（G8），`preview_text_tokens=384`；原始 2KB 入 `session_artifacts(storage_backend='edge-local')`；
  - `stable_prefix_hash` 与 T01 相同（system+tool_schemas 未动）→ prompt cache 命中。
- **User sees**：Context 侧板显示一条 tool_call chip "fs.read manifest.json → 384 token preview"；主对话区 agent 回复开始逐条映射合规 checklist。
- **Manifest zones**（ctx_s14_0002, total=7630）：tail +180→2130、tool_previews 首次 =384、retrieved 压到 850。`reason='turn_start'`。

### T03 — 10:15 地铁切 iPhone：cloud + 冷启

- **User**：上地铁，iPhone Safari 首次打开 astra，URL 复制过来。
- **Server**：
  - 客户端 `GET /sessions/s_helper_v2_0506/state` **无 `known_state_revision`**（IndexedDB 空） → G13 "gap full reset"：下发全量 hot projection；
  - `auth_sessions` 写新行 `device_id=iphone_xxx, device_fingerprint=fp_iphone_yao_3f`；`session_device_leases` 插入 `trust_level='new_device'`（G13 L487）；
  - A14 workspace 切换：写 `session_state_items(category='workspace_state', item_key='authority', status='active', payload_json='{"authority":"cloud","workspace_id":"ws_cloud_yao","sandbox_id":"sbx_yao_0506","edge_status":"offline","reachability_probe":{"last_ok_at":"2026-05-06T08:35Z","last_fail_at":"2026-05-06T10:15Z","probe_method":"HEAD","rtt_ms":null},"tool_whitelist":["sandbox.fs.read","sandbox.fs.write","sandbox.shell","web.search","artifact.fetch","plan.update","ask_user"]}')`；
  - 发 `workspace_reachability_changed` 事件（G11 L2698）；
  - `monotonic_id` 47→48；`revision_hash` 重算包含 `fp_iphone_yao_3f`。
- **User sees**：iPhone 首屏等 1.8s（全量 hot projection ~40KB 压缩 ~12KB 过 3G）；Context tab 顶部红底提示「Cloud sandbox — 本地 repo 不可读」。
- **Manifest zones**（ctx_s14_0003, total=7363）：**tool_schemas 700**（7 个 cloud schema，节省 1000）、recent_tail **扩到 2800**（tool_schemas 让位）、workspace zone **120**（扩到 "cloud_sandbox ws_cloud_yao; edge offline; user must push/paste local code"）。`reason='workspace_switch'`（G1）。

### T04 — 10:18 Agent 撞墙 → 加 workspace_constraint hint

- **User**：「继续看下 background.js 那段消息监听。」
- **Server**：
  - 无 `fs.read`（只有 `sandbox.fs.read`，读的是 cloud sandbox 的空目录） → agent 不能盲试；
  - `session_anchor` 下一轮注入 G11 L2700 要求的 workspace 约束句（"cloud sandbox: no local repo access"）；
  - 回复走 `ask_user` 要用户手动搬代码。
- **User**：「我没 commit，就 50 行你等一下我贴。」5s 后贴 47 行 JS (1.8KB)。
- **Server**：
  - 贴码入 `session_artifacts(storage_backend='matrixone_inline')`，`preview_template='paste_code_v1'`（兜底 400 char）；
  - `session_state_items(category='error_state', item_key='edge_unavailable_on_cloud', status='resolved_by_paste')`；
  - `session_state_item_events.mutation=insert`。
- **User sees**：小姚在 iPhone 上 3 段式贴：首段 copy 后切 astra 标签、编辑区点进文本框贴、按发送 —— 整个过程 ~25s。
- **Manifest zones**（ctx_s14_0004, total=7510）：recent_tail +520→2700（贴码 preview），tool_previews 仍 0。贴近红线。

### T05 — 10:30 歧义爆点：「上周那个方案不是说也能行吗？」

- **User**：直接扔过来，没指向，手里还端着咖啡。
- **Server**：走 G12 三级：
  1. **L1 structured**：`agent_runs` 无 waiting（T02 已 resolve），`session_state_items(category='suggested_next_action', status='pending')` 只有 T04 的 error_state 非 next-action → miss；
  2. **L2 rule**：utterance 无明确 ref（无 turn_idx、无 todo_id、无技术名词） → miss；
  3. **L3 small-model**：7B 本地模型，输入 ~600 token（最近 5 轮 compact + 上周三条相关 decision），输出 3 candidates JSON；confidence 分别 0.62/0.58/0.41，均 <0.8；
  4. 按 G12 L686 **一轮最多 5 条 suggestion**，这里 3 条 OK；最高 0.62 落在 0.5–0.8 区间 → 列候选 ask_user；
  5. 写 3 行 `session_state_items(category='suggested_next_action', item_key='resolve_ambiguity_lastweek_plan.{A,B,C}', status='pending', expires_at=NOW()+1h, payload_json='{source:small_model, confidence:0.62, source_turn:128}')`（G12 hint 默认 1h）。
- **User sees**：agent 不瞎 fs.read，直接：「你指的是下面哪个？A: IndexedDB（0.62）/ B: chrome.storage.sync（0.58）/ C: Service Worker（0.41）」并列出每个的 2 行摘要。
- **Manifest zones**（ctx_s14_0005, total=7185）：**retrieved_facts 压到 1000 极限**（装 3 candidate summary + artifact_ref 指向 turn 128/102/141），其它 11 条 retrieved drop，`dropped_count=14`。**用 T5 做 §7 详算**。

> **7B 可靠性隐忧**：qwen-2.5-7b-int4 跑 JSON schema 约束抽取，实测漏 `confidence` 字段的概率 ~8%；v0.2 未规定 small-model 输出的 schema 校验失败兜底 —— 见 §5 候选 gap。

### T06 — 10:33 用户选 A

- **User**：「A。」—— 1 个字母。
- **Server**：
  - L1 structured 命中：有 pending suggestion `resolve_ambiguity_lastweek_plan.A`，用户单字母符合「选项字母」rule（G12 L670 rule-backed 0.95 confidence） → accepted；
  - `session_state_item_events.mutation=apply_suggestion, ref=sna_A`（G14）；其它两个候选 `status=pending→rejected_by_selection`；
  - 读 turn 128 的 artifact_ref → IndexedDB 方案全文 820 token，但 retrieved zone cap 1000，通过 `PromptRenderer.render_mode='compressed_summary'` 压到 680 token，保留 `source_hash` 便于后续回补。
- **User sees**：agent 立刻进下一步「我用 IndexedDB 开搭 storage 层，先列接口签名…」；Context tab 候选 B/C 打灰底。
- **Manifest zones**（ctx_s14_0006, total=7488）：retrieved=680（方案 A 压缩文本）、tail +180。`stable_prefix_hash` 未变 → prompt cache 继续命中。

**T06 SQL 伪代码（apply_suggestion 事务）**：

```sql
-- 1) 命中 pending suggestion
SELECT item_id, payload_json FROM session_state_items
 WHERE session_id=:sid AND category='suggested_next_action'
   AND item_key='resolve_ambiguity_lastweek_plan.A'
   AND status='pending' AND expires_at > NOW()
 FOR UPDATE;
-- 2) 状态机推进 pending→accepted，其它候选→rejected_by_selection
UPDATE session_state_items SET status='accepted', updated_at=NOW()
 WHERE item_id=:a_id;
UPDATE session_state_items SET status='rejected_by_selection', updated_at=NOW()
 WHERE session_id=:sid AND category='suggested_next_action'
   AND item_key IN ('resolve_ambiguity_lastweek_plan.B','resolve_ambiguity_lastweek_plan.C')
   AND status='pending';
-- 3) 三条 events：apply_suggestion + 2 * update
INSERT INTO session_state_item_events(event_id, session_id, item_id, mutation, payload_json, ...)
 VALUES (..., :a_id, 'apply_suggestion', '{"ref":"sna_A","chosen_by":"user","confidence_at_accept":1.0}'), ...;
-- 4) state_revision 推进，revision_hash 重算（含 device_fingerprint=fp_iphone_yao_3f）
UPDATE agent_sessions SET monotonic_id=monotonic_id+1, revision_hash=:new_hash WHERE session_id=:sid;
```

### T07 — 10:45 「plan 给我看看」— 12 条塞 400 token

- **User**：「把 plan 给我看看。」
- **Server**：
  - G5 Plan Tree Rendering Policy 入场（v0.2 L2249–2295）；
  - `session_todos` 12 条：root 有 3 个 section（合规 / 实现 / UI），当前在 section="实现" 下；
  - 按 G5 规则：
    - 祖先链：`root > 实现` 两节点 + 完整 title + status + 1-line summary；
    - 当前 section 的 active/pending subtask 全渲（IndexedDB storage / IndexedDB 迁移 / error 处理）；
    - 兄弟 section "合规"（done）、"UI"（pending）只渲 title+status；
  - Token 估算：祖先链 80 + 当前 3 subtask 240 + 2 兄弟 60 = 380，进 plan_todo 400 cap 刚好；
  - 剩 20 token 写尾注 "(5 more todos archived/done — query subtree for details)"；
  - 未 render 的 5 条写 `context_manifest_items(included=false, source_id=todo_xxx, reason='plan_tree_sibling_compression')`（G1 枚举）。
- **User sees**：Context tab 右侧展 "Plan tree (click to expand subtree)"；聊天区 agent 报 3 条 active 的执行顺序。
- **Manifest zones**（ctx_s14_0007, total=7480）：plan_todo=380、tail 回落到 2000（腾地给 retrieved 保留 A 方案）。

> **关键点**：G5 是**结构化渲染**而非"截断"，子树可通过 `GET /sessions/{id}/plan/subtree?root=section_ui`（L2263）拿回。8k 窗下 plan_todo 400 token 够用（恰是 G5 + G10 配合的结果），无需临时借预算。

### T08 — 14:00 到公司工位 PC：hybrid 恢复

- **User**：到公司，开 Chrome 登 astra，`state_revision.monotonic_id=41`（上周的 stale IndexedDB）。
- **Server**：
  - `GET /sessions/.../state?known_state_revision={monotonic_id:41,hash:...,device_fingerprint:fp_pc_yao}` → G13 比 41，retention 窗内 → 下发 42..48 + 49 delta；
  - `session_device_leases` PC 设备 `trust_level` 恢复 `trusted`（上周 lease 虽过期但 revoke_at 为 NULL，直接续 14 天）；
  - A14 workspace：检测 `bridge_pc_yao` 在线 → probe HEAD rtt=12ms → `edge_status='online'`；但 `edge_bridge_id=bridge_pc_yao` ≠ T01 的 `bridge_mac_yao`，`workspace_fingerprint` 变化 → 新建 `workspace_state` 记录；
  - `tool_whitelist` 回到 15 项（edge+cloud 联合）；
  - `monotonic_id` 48→49，`revision_hash` 包含 `fp_pc_yao`。
- **User sees**：PC 上 200ms 内 delta 吐出，比 iPhone 冷启顺 5×；Context tab 顶部由红变绿 "Hybrid workspace 已恢复"。
- **Manifest zones**（ctx_s14_0008, total=7575）：tool_schemas 700→1700、tail 2700→2000（drop T04 贴码 520 的原文，留 artifact_ref，`reason='older_turn_has_artifact_ref'`，G1）、tool_previews=320（T02 保留的 manifest.json preview 被 promote 回来，预备用户问文件结构）。

### T09 — 14:20 纯歧义：「这个方案不行换一个」

- **User**：看了 agent 刚写的 storage 层代码，嫌 IndexedDB promise wrapper 啰嗦。
- **Server**：
  - L1 structured：无 waiting；
  - L2 rule：「这个方案」无指向 → miss；
  - L3 small-model：2 候选 `{IndexedDB（T05选的）, 内网同步策略（T08 讨论）}`，confidence 0.55/0.48；
  - **ask-user 疲劳检查**（G12 L693）：查过去 1h 内本 session 的 ask_user 次数 = 2（T04 贴码 ask、T05 候选 ask），本次是第 3 次 → **降一档阈值** from 0.8 to 0.5；但注意 G12 L695 规定 "small-model-only candidates still require user confirmation"，所以 **疲劳降档对本轮无效**，仍走 ask_user；
  - 写 `suggested_next_action` 两行 pending，`expires_at=NOW()+1h`。
- **User sees**：agent 坦白："我不确定你想换 IndexedDB 整体方案，还是只是 wrapper 写法？A: 换 IndexedDB；B: 保留方案，换 wrapper 风格。"
- **Manifest zones**（ctx_s14_0009, total=7260）：retrieved=700、tool_previews 320 保留、tail 2100。按题目硬约束 **≤7300** 通过。

### T10 — 15:40 小姚选 B + 继续开干（递进式加载）

- **User**：「B 吧，不换方案，wrapper 换得简洁点。继续写 storage 层 + 迁移 + 错误处理。」
- **Server**：
  - apply_suggestion.ref=sna_B；
  - 调 edge `fs.write(src/storage/indexed_db.ts)`、`eslint.run` —— 4 个 tool 链：`fs.read existing`、`fs.write`、`eslint.run`、`puppeteer.screenshot(popup.html)` 校 UI；
  - eslint 输出 1800 行，`preview_template='eslint_v1'`（G8），preview 压到 320 token，原文 artifact；
  - 单轮要同时看「方案 A 原文 680 + eslint preview 320 + background.js 贴码 520 + storage skeleton 600」= 2120 token，但 retrieved(1000) + tool_previews(500) 只 1500；
  - 走 **progressive loading**：第一轮 manifest 只放 A 压缩 + eslint preview + skeleton 摘要；background.js 贴码降级为 `artifact_ref`，等下轮用户追问再 fetch；
  - `context_manifests.reason='turn_with_progressive_loading'`（G1 枚举需增，见 §5）或退到 `large_tool_output_gated`；
  - `dropped_count=21`，每个 drop 带 `source_id + artifact_ref` 备查。
- **User sees**：流式输出：5 行 skeleton → tool chip "eslint.run (3 warnings)" → 继续写迁移逻辑；屏幕下方 progress dot 显示"4 tools, 2 artifacts"。
- **Manifest zones**（ctx_s14_0010, total=7490）：retrieved=780 (A 压缩+skeleton 摘要)、tool_previews=440 (eslint 320 + puppeteer 120)、tail 压到 1800（倒数第 3 轮做 L1a compress 把贴码转 artifact_ref）。

### T11 — 18:30 Chromebook VPN：edge 侦测盲区 → 60s hang

- **User**：客户现场，Chromebook 登 astra。公司 VPN 让 DNS 解析 `bridge-yao.local` 返回 split-tunnel 假 IP，但 TCP 443 建链 SYN 没回。
- **Server**：
  - Chromebook IndexedDB 空 → gap full reset；新 `device_fingerprint=fp_chromebook_yao_7a`，`trust_level='new_device'`；
  - 客户端上报 `workspace_hint=edge`（因 DNS 通），服务端**必须**按 G11 L2697 "cloud relay 200ms HEAD probe before forwarding edge tool"；
  - 但**此时**尚未触发 edge tool call，只是打开 session → 服务端按 workspace_state 乐观标 `edge_status='reconnecting'`（G11 L2690 "transient failure within grace window"），tool_schemas zone 把 edge-only 工具**隐藏**但不写入 cloud-only；
  - 用户说"继续写那个 storage 的错误处理"，agent 要 `fs.read(src/storage/indexed_db.ts)` —— cloud relay 发 HEAD probe，200ms 超时 → probe fail；
  - 服务端写 `workspace_state.edge_status='offline'`, `reachability_probe.last_fail_at=NOW()`，`authority` 从 edge 降到 cloud，写 event `workspace_reachability_changed` + `workspace_downgraded`；
  - **但**！G11 L2697 讲的是"cloud relay 转发前先 probe"，这只挡了一次 60s hang —— 实际执行是 200ms HEAD 立即失败 → 立即降级 → rebuild tool_schemas → 重发 LLM 请求。F1（S14 §7）在 v0.2 下**已 closed**；
  - `monotonic_id` 51→52；tool_schemas 15→7；写 `error_state.edge_probe_timeout` active。
- **User sees**：Chromebook 打开后等 2.2s 拿到全量 state + 初始 manifest；小姚问后 ~800ms 收到回复"我这边到你工位 edge 不通，我退成 cloud 了，有两条路：(a) 你把 storage 文件贴给我 (b) 我用 T10 留的 skeleton 继续推"。**无 60s hang**。
- **Manifest zones**（ctx_s14_0011, total=7065）：tool_schemas=700、tail=2500（Chromebook 冷启没有本机 tool preview 可复用）、workspace=120（写明"edge_reachable=false, prev_authority=edge, downgrade_reason=edge_probe_timeout"）。

**T11 cloud relay probe 伪代码**：

```rust
// astra cloud relay, 接到 fs.read(src/storage/indexed_db.ts)
async fn forward_edge_tool_call(session_id, tool_call) -> Result<Output> {
    let ws = load_workspace_state(session_id).await?;
    match ws.edge_status {
        Online => {} // G11 之前 probe 过 200ms 内
        Reconnecting | Offline => {
            // 先 200ms HEAD probe，L2697
            let probe = tokio::time::timeout(
                Duration::from_millis(200),
                http_head(format!("http://{}/health", ws.edge_bridge_id))
            ).await;
            match probe {
                Ok(Ok(_)) => mark_edge_online(session_id, rtt).await?,
                _ => {
                    // probe fail → 降级
                    write_state_items(session_id, workspace_state, cloud_degraded).await?;
                    emit_event("workspace_reachability_changed", {from: edge, to: cloud}).await?;
                    rebuild_tool_schemas(session_id, cloud_whitelist).await?;
                    return Err(EdgeUnreachable); // 让上层重走一轮 LLM
                }
            }
        }
        Detached => return Err(RequiresReattach),
    }
    // probe OK → 正常转发
    forward_to_bridge(tool_call).await
}
```

可见 F1（S14 原文担心的 60s hang）在 v0.2 下**被 G11 L2697 的 200ms HEAD cap 住了**；真正的遗留问题是 F-5（session 开局时是否 probe 未写死）和 F-8（reconnecting 期间 tool_schemas 的决策）。

### T12 — 19:45 交付收尾

- **User**：「就 T10 那版 skeleton，补 README + changelog，我回去 MacBook 合。」
- **Server**：
  - 仍 cloud；agent 生成 `README_v2.md`（1.2KB）、`CHANGELOG_v2.md`（0.4KB），两份入 `session_artifacts(storage_backend='matrixone_inline', retention_policy='project_long_term')`（G9）；
  - **严格遵守 A14**：cloud sandbox 的 `tool_whitelist` 不含 `git.commit`，agent 不尝试；
  - 写 `session_state_items(category='suggested_next_action', item_key='user_commit_merge', status='pending', expires_at=NOW()+24h, payload_json='{action:git_commit+push+pr, target_device:MacBook(fp_mac_yao), files:[README_v2.md,CHANGELOG_v2.md,src/storage/indexed_db.ts]}')`，**confidence=1.0 (structured-backed by tool_whitelist constraint)**，但按 G12 是 approval 型 action → expiry 24h；
  - `agent_runs.r_v2_y6` status=completed；写 `summary.day_progress_0506` active（240 token，下次开 session 进 anchor）；
  - 审计：12 条 context_manifests、5 条 workspace_state 变更（T03/T08/T11 主变更 + T04/T11 error_state）、5 条歧义识别 event、0 条客户端污染（A1 保持）。
- **User sees**：聊天底部 banner "已交付：2 artifact 云端保存，切回 MacBook 后会看到『还有一步要你亲自合』卡片"；小姚 19:58 回家路上在地铁上点开 iPhone —— `suggested_next_action.user_commit_merge` 的"卡片" zone 占 80 token 放在 anchor 里。
- **Manifest zones**（ctx_s14_0012, total=7190）。

---

## §4 发现的别扭点（Frictions）

### F-1 小模型 JSON 抽取 schema 校验失败兜底未规定
- **Ref**: T05 / G12 §Next-Action Confidence State Machine
- **Observation**: v0.2 L680–684 状态机默认 small_model 一定会返回合法 `{candidates:[{id,summary,source_turn,confidence}]}`。但 qwen-2.5-7b-int4 实测漏字段概率 5–10%，尤其 `confidence` 字段；也可能返回只有 1 个候选或 6 个候选（违反 L686 `max=5`）。
- **Why friction**: 实现层要做"JSON 解析失败 → retry 一次 → 仍失败 → 回退到 ask_user 裸文本"的三级兜底，v0.2 无字。工程师要么自己编策略（可能各做各的），要么等 G12 补。
- **Severity**: `notable`

### F-2 progressive_loading 不在 G1 manifest reason 枚举里
- **Ref**: T10 / G1 §Context Manifest Reason Enum
- **Observation**: 场景原文用 `reason='turn_with_progressive_loading'`，但 GAP-TRACKER G1 patch 列的 reason 里没这个（L52：initial_turn/post_compaction/history_recall_*/large_tool_output_gated/plan_subtree_query/tree_structured_report/workspace_switch/approval_resume/cross_session_recall）。
- **Why friction**: 小窗口下**递进式加载**是 first-class 现象（T10 一次、S14 §附录 B 声明），但没法写进 reason。退而求其次只能复用 `large_tool_output_gated`，但这不精确 —— progressive loading 未必触发于大 tool output，可能是 retrieved + preview 合计超限。
- **Severity**: `notable`
- **建议**: G1 枚举补 `progressive_loading` 一行。

### F-3 ask-user 疲劳降档对 small-model-only 候选**无效**，但用户不知道
- **Ref**: T09 / G12 L693–696
- **Observation**: L693 "1h 内 3 次 ask → 降一档"，L695 "small-model-only candidates 仍要用户确认"。T09 是第 3 次 ask，场景作者以为会降档自动执行，但 v0.2 规定明确"对 small-model-only 不降"。小姚并不知道 agent 在"能自动跑但选择不跑"还是"真不行"，心理负担依旧。
- **Why friction**: 设计是**对的**（安全优先），但**用户体验上**会显得"这 AI 怎么还在问"。v0.2 无字说 agent 回答时是否应解释"因为我只从 small model 猜到，需要你确认"—— 解释了用户理解，不解释用户抱怨。
- **Severity**: `minor`
- **建议**: 在 `ask_user` 消息里自动附一行出处（"这是我猜的，不确定"），让 UX 显式承认。

### F-4 iPhone 冷启全量 hot projection 体积预估缺
- **Ref**: T03 / G13 "gap full reset" + §Web Client Cache
- **Observation**: v0.2 L477–481 规定 gap 全量重置下发 hot projection，但**没写** hot projection 体积 cap。S14 session 有 12 个 todos + 47 个 state items + 5 个 artifacts + 最近 20 条 transcript summary，估 40KB 压缩 12KB。iPhone 4G 1–2s 可接受，但跨洋 /Starlink 用户呢？
- **Why friction**: 无 cap 导致客户端"首开即卡"；也无"分页下发 hot projection"的 API。
- **Severity**: `minor`（但移动场景高频）

### F-5 T11 Chromebook 首次 tool call 是否触发 200ms probe 有歧义
- **Ref**: T11 / G11 L2697–2700
- **Observation**: 设计写"cloud relay 在**转发 edge tool call** 前先 200ms HEAD probe"。问题是：**打开 session 时**（T11 起点）有没有 probe？如果没有，manifest ctx_s14_0011 第一轮已经把 edge-only tools 放进 tool_schemas，**第一次** tool call 才触发 probe → 第一次 tool call 会因 probe fail 而**不被调用**（agent 要重写 manifest），这是"多一轮 LLM 请求"的开销。如果有，那么每次设备切换都要一次 200ms 延迟 —— 小姚打开 Chromebook 多等 200ms 可接受，但 iPhone 4G 下 rtt 可能 400ms+。
- **Why friction**: G11 只讲 tool 转发前的 probe，没讲 **session open**/state projection 时是否 probe。
- **Severity**: `notable`
- **建议**: G11 补"workspace_state 创建或 `device_fingerprint` 变化时必须 probe 一次；结果写 `reachability_probe`"。

### F-6 T05 retrieved zone 1000 token 装 3 candidate + source ref 实测吃紧
- **Ref**: T05 / G10 L2422
- **Observation**: 3 个候选 summary 各 ~180 token = 540，加 3 个 `artifact_ref` 各 40 token = 120，candidate metadata (confidence/source_turn) 各 20 token = 60，合计 720。还剩 280 token 预备"每个候选的关键差异句"—— 用户要看"为什么这 3 个不同"才能选得明白。如果候选是 5 个（G12 L686 允许最多 5），1000 token **不够**。
- **Why friction**: G10 的 retrieved=1000 和 G12 的 max 5 suggestions 预算冲突。
- **Severity**: `notable`
- **建议**: G10 写 "当本轮是歧义 ask_user 专用 render 时，retrieved 可临时扩到 1500，从 recent_tail 借"。

### F-7 `workspace_fingerprint` 与 `edge_bridge_id` 的重复/不一致
- **Ref**: T08
- **Observation**: S14 §3.T08 写 "workspace_fingerprint 与 T01 MacBook edge 不同"；v0.2 L2675 用 `edge_bridge_id`。两者什么关系？是 alias 还是两个字段？实现时不明。
- **Severity**: `minor`

### F-8 Chromebook `workspace_hint=edge` 乐观标注期的 tool_schemas 窗口期
- **Ref**: T11 / G11 "reconnecting" 状态
- **Observation**: T11 开始 Chromebook 上报 `workspace_hint=edge`，服务端按 G11 L2690 "reconnecting" 标记 —— 此时 tool_schemas zone 是**按 edge 生成** 还是**按 cloud 生成**？v0.2 规定 "hidden from new LLM turns"（L2691），可读为"reconnecting 时不把 edge-only tool 送 LLM"。但实现者很容易读成"保留 edge tool schema 但 LLM 调用就失败"。如果选前者，第一轮 manifest 要按 cloud 生成，等 probe 成功再扩；如果选后者，浪费 1000 token。
- **Why friction**: 语义虽然写了，实现两条路都合乎字面，需要**一句 normative** —— "reconnecting 期间 tool_schemas zone 按 `authority='cloud'` fallback 生成"。
- **Severity**: `notable`

### F-9 `revision_hash` 包含 device_fingerprint 的副作用：同一 monotonic_id 多 device 不同 hash
- **Ref**: T03 / T08 / T11 / G13 L446–451
- **Observation**: `revision_hash = sha256(session_id, monotonic_id, device_fingerprint, tx_wm, re_wm, projection_hash)`。同一 server monotonic_id=48，在 iPhone 看到的 hash 与在 MacBook 看到的**不同**（device_fingerprint 不同）。这对 G13 "if-none-match" 比 monotonic_id 没问题；但**客户端显示** "同步于 rev_hash=xxx" 时，用户在不同设备看到不同字符串 —— UX 上会想"我到底同步完了没"。
- **Why friction**: 设计是正确的（hash 区分设备）但**客户端文案**需要处理，不能把 hash 当 human-readable id 展示。v0.2 未禁止 UI 直接显示 hash。
- **Severity**: `minor`

### F-10 `agent_runs.waiting_for` 字符串未枚举，T01 structured 命中靠字符串匹配
- **Ref**: T01 / G12 L676–677
- **Observation**: G12 状态机第一条 "structured_event hit → accepted"，实现上是查 `agent_runs WHERE status='waiting_for_user'` —— 但 T01 的"继续"要对应到 `waiting_for='user_question:compliance_raffle'` 这个具体字符串。字段没枚举、没索引，直接匹配就是字符串比较。如果昨晚 agent 写 `waiting_for='user-question:compliance-raffle'`（横杠 vs 冒号），T01 的 structured 匹配就 miss → 降级到 L2 rule → 再 miss → L3 small-model → 小模型猜"继续"指什么，可能猜成某个 pending todo 而非 waiting_for。
- **Why friction**: v0.2 没规定 `waiting_for` 字符串格式契约；G12 靠它走 L1 命中路径。
- **Severity**: `notable`
- **建议**: G12 或 G7 补 `waiting_for` 格式为 `<category>:<item_key>` 强契约。


---

## §5 新 gap 候选

**候选 G20：small-model 抽取器 JSON schema 校验失败兜底契约缺**
- **title**: Small-model next-action extractor needs JSON schema validation + retry/ask fallback contract
- **triggered_by_turn**: T05 / T09
- **related_v0.2_section**: §Next-Action Confidence State Machine (L657–696)
- **severity**: medium
- **rationale**: G12 假设 small_model 一定返回合法 JSON，但在 7B int4 量化模型上实测失败率 5–10%。现在设计留白，各实现会各自写兜底 —— 有的直接 crash，有的偷偷降级为 ask_user 但不写 event，**失去可审计性**（违反 A15 的初衷）。这是一个独立 gap，不是 G12 的实现细节，因为涉及 contract：失败是否要写 `agent_run_events.next_action_extraction_failed`？retry 几次？retry 时 budget 怎么算？
- **proposed_fix**: G12 补 "Small-Model Output Validation" 子节：(1) 定义 JSON schema； (2) 校验失败 retry 最多 1 次；(3) 仍失败 → 强制走 ask_user 裸文本 + 写 event `next_action_extraction_failed`；(4) 单轮 retry 成本计入 ask-user 疲劳计数。

**候选 G21：manifest reason 需补 `progressive_loading`**（F-2 衍生）
- **title**: `context_manifests.reason` enum missing `progressive_loading` for small-window multi-content turns
- **triggered_by_turn**: T10
- **related_v0.2_section**: §Context Manifest Reason Enum (G1)
- **severity**: low
- **rationale**: 小窗口下一轮要同时看 4+ 来源的 content 是 S14 特有常见模式（budget_v1_8k 下 retrieved+preview 合计 1500 很容易不够），不应被挤进 `large_tool_output_gated`。
- **proposed_fix**: `context_manifest_reason_types` 加一行 `progressive_loading, reason_class='budget', default_zone=null`。

**候选 G22：edge probe 触发时机 + reconnecting 期间 tool_schemas 回退行为**（F-5 / F-8 合并）
- **title**: Edge reachability probe must fire at session-open and device-fingerprint change; `reconnecting` window must pin tool_schemas to cloud fallback
- **triggered_by_turn**: T11（也影响 T03 / T08）
- **related_v0.2_section**: §Workspace Reachability and Degradation Semantics (L2665–2716)
- **severity**: medium
- **rationale**: G11 L2697 只规定 "cloud relay 转发 edge tool call 前 probe"。但 session 开局（如 T11 Chromebook 冷启）和 device_fingerprint 变化（T03 iPhone / T08 PC）时**不转发 tool 也需要 probe**，否则 `workspace_state` 里的 `edge_status` 永远停在上次的值，manifest 的 tool_schemas 按错 authority 生成。另外 reconnecting 状态下 tool_schemas 按 edge 还是 cloud 生成 v0.2 未写死。
- **proposed_fix**: G11 补一小节 "Probe Triggers"：① session open (fingerprint changed)、② 首个需要 edge 的 tool call、③ workspace authority change 事件 — 三种触发都先 probe 200ms；reconnecting 期间 tool_schemas 一律按 `authority='cloud'` 降级版生成，UI 显示"edge 正在重连"。

**候选 G23：`agent_runs.waiting_for` 格式强契约**（F-10 衍生）
- **title**: `agent_runs.waiting_for` needs a normative string format `<category>:<item_key>` for G12 L1 structured match
- **triggered_by_turn**: T01
- **related_v0.2_section**: §Durable Runs / §Next-Action Confidence State Machine
- **severity**: low
- **rationale**: G12 L1 命中结构化 event 依赖字符串比对，若两个实现把冒号写成横杠就 miss，用户看来"AI 忘了昨天在干什么"。应该是 contract 而非风格。
- **proposed_fix**: `agent_runs.waiting_for` 加校验 regex `^[a-z_]+:[a-z0-9_.-]+$`；category 来自 `session_state_items.category` 白名单。

---

## §6 Gap 组合效应检验

### 组合 1：G10 + G18（小窗口 × delegation zone 没位置）
- **运行时可能发生的坑**: S14 本身是单 agent，但 v0.2 规定 `delegation_state` 是 first-class zone，总 cap 1500（L2447）。若 S14 在 budget_v1_8k (7500 cap) 下同时跑 delegation 子 agent，剩余可用 = 7500 - 1500 = 6000，而 G10 定 system+tool_schemas=3400、anchor 200、plan 400、summary 500、retrieved 1000、tail 2000（floor 1600）—— 合计 7500 - (3400+200+400+500+1000+2000) = 0，**delegation zone 无预算可借**。
- **v0.2 是否覆盖**: `no`
- **如果 no，建议**: G10 补 "budget_v1_8k 下 delegation zone 默认关闭；如需启用，tail floor 从 2000 降至 1200，retrieved 从 1000 降至 600，delegation cap 从 1500 降至 800"。

### 组合 2：G11 + G13（edge reattach × device lease）
- **运行时可能发生的坑**: T08 工位 PC `bridge_pc_yao` 上周登记过，`session_device_leases.trust_level` 过期待 reattach。G11 L2702 "reattach protocol" 要求 bridge 出示 `edge_bridge_id + device_fingerprint + user auth`；G13 `session_device_leases` 用 `device_id`（VARCHAR 128） — **两者 fingerprint 同不同？** 若浏览器 fingerprint ≠ edge bridge fingerprint（是两台物理设备 —— 同一台 PC 的 Chrome 进程 + edge bridge daemon），两套 lease 会各自发散。T11 Chromebook 无 bridge，只有浏览器 lease，走单边；T08 PC 两个 lease 都有，但不知道谁是谁的"主"。
- **v0.2 是否覆盖**: `partial` —— G13 定义了 session_device_leases，G11 定义了 edge_bridge_id，但**两表没 FK**。
- **如果 no，建议**: G13 `session_device_leases` 加 `bridge_id VARCHAR(128) NULL` 指向 edge bridge；G11 reattach 时同时 touch 两表；或独立 `edge_bridge_leases` 表避免混同。

### 组合 3：G12 + G10（疲劳降档 × 小窗口 retrieved 装不下候选差异句）
- **运行时可能发生的坑**: G12 1h 内 3 次 ask → 降档。但 **降档触发条件**依赖"候选是 structured/rule-backed"（L694）—— 要判定这个又要读 candidate metadata，小窗口下 retrieved zone 1000 token 已经紧（F-6），候选的 `source` 字段可能因压缩被 drop。结果：疲劳策略要降档，但读不到 candidate source，只能保守**不降**，疲劳策略在 S14 实际上**永远不生效**。
- **v0.2 是否覆盖**: `no`
- **如果 no，建议**: candidate `source` 字段是一级属性，压缩时必须保留（G10 / G12 联合补 "retrieved zone 里的 candidate metadata 属不可压缩字段"）。

### 组合 4：G13 + G16（device fingerprint × per-session skill 激活）
- **运行时可能发生的坑**: S14 本身没用 skills，但假设小姚在 MacBook 上激活了 `go-code-review` skill（T01），`active_skill` 是 `scope='session'`（L1144 / L2131）。切到 iPhone (T03)，新 device_fingerprint，G13 gap full reset 全量下发。但 `session_state_items(category='active_skill')` 是 **session 级**，不是 device 级，理应带过去。**但** skill version_id 冻结（G16 L2145）于激活时，MacBook edge workspace 里这个 skill 是 `scope='user'+'workspace'='edge_local'`，切 cloud 后 `scope` 查询路径变了，可能找不到 skill content。
- **v0.2 是否覆盖**: `partial` —— G16 L2147 规定"新 session 不自动加载 installed skill"，但**同一 session 换 workspace**时的行为没定。
- **如果 no，建议**: G16 补一条 "active_skill 与 workspace scope 正交 —— 激活后 skill content 已 freeze 到 session 级 payload，workspace 切换不影响加载"。

### 组合 5：G5 + G10（plan tree render × plan_todo 400 token 小窗口）
- **运行时可能发生的坑**: G5 L2257 "always render ancestor chain with full title, status, and one-line summary"。若 S14 plan tree 深到 4 层（S06 场景里有 4 层），ancestor chain 就是 4 个节点 ×（title 30 + status 8 + 1-line summary 40）≈ 312 token，加当前 section 3 subtask 240 = 552，**已超 plan_todo cap 400**。G10 小窗口下 plan_todo 硬 400，不留腾挪；G5 又要求 ancestor chain 必渲。
- **v0.2 是否覆盖**: `no` —— G5 和 G10 分别写，但未说小窗口下如何让步。
- **如果 no，建议**: G5 补 "当 plan_todo zone cap < 600 token 且 depth≥3 时，ancestor chain 只渲 path 字符串（materialized path L2297）+ 当前 2 级的 full title/status，其余仅 path 标示"。

---

## §7 Token 实际 walkthrough（T05 最紧张：cloud + 3 candidate ask）

**选 T05 原因**：iPhone 冷启 + cloud sandbox（tool_schemas 降到 700 为 tail 腾地）+ 小模型抽取刚吐 3 候选 + retrieved zone 被压到 **1000 极限**（场景原文 §3.T05）。这是 S14 里 zone 分配最"拧"的一轮。

**预算基线**（`budget_v1_8k`, v0.2 L2418–2425）：
- context_window_tokens = 8192
- reserved_output = 500（L2425）
- safety_buffer = 200（L2426）
- 实际可用 content = 8192 - 500 - 200 = **7492**
- 题目硬约束：本轮 **total_content ≤ 7300**（留 192 给 tokenizer 漂移误差 + 系统级字段，比 v0.2 cap 更严）

**T05 逐 zone 实际占用**（估算，`tokenizer_id=qwen2.5-int4-bpe-v1`）：

| Zone | Cap | 实际 | 说明 |
| --- | ---: | ---: | --- |
| system_static | 3400 总限 | **1700** | v0.2 安全契约 + 工具使用指引裁短版 |
| tool_schemas (cloud, 7 tools) | 3400 总限 | **700** | `sandbox.fs.read/write/shell`、`web.search`、`artifact.fetch`、`plan.update`、`ask_user` 精简 schema |
| session_anchor | 200 | **195** | 目标"Chrome 插件 v2 交付" + 合规白名单 5 条（L2418 cap 200 严卡） |
| plan_todo | 400 | **380** | 3 active todo full（合规 done / IndexedDB in_progress / icon pending）+ "(9 more)" 兜底（G5 未触发 because 只看 active） |
| recent_tail | 2000（可扩 2800） | **2600** | 扩到 2600（tool_schemas 让出 1000 中的 600 给 tail，剩 400 给 retrieved buffer）；装 T01–T04 4 轮 |
| summary | 500 | **490** | 昨天 L1b 浓缩 "v1 完成 / v2 插件骨架 done / 抽奖合规待 PM 确认" |
| retrieved_facts | 1000 | **1000** | **满**：3 candidate summary 540 + 3 artifact_ref 120 + 3 confidence+source_turn 60 + 3 差异句 180 + header 100 = 1000 |
| tool_previews | 500 | **0** | T04 贴码在 tail 里，T02 manifest.json preview 在 iPhone 冷启后未 promote |
| workspace_state | — | **120** | "cloud_sandbox; no local repo; edge offline since 10:15" |
| delegation_state | 1500 | **0** | 无子 agent |
| **content 合计** | 7492 | **7185** | ≤ 7300 ✓，剩 115 给 tokenizer drift |

**验算**：1700 + 700 + 195 + 380 + 2600 + 490 + 1000 + 0 + 120 + 0 = **7185**

**加 reserved/safety**：7185 + 500 (output) + 200 (safety) = **7885 ≤ 8192** ✓，剩 **307 token buffer** 给 tokenizer drift（qwen 与 GPT-4o BPE 差 ~12%，7185 × 12% = 862，但 in-family 漂移通常 <3%，307 够）。

**drop 分析**：
- 被 drop 的 11 个候选 retrieved_facts（G1 reason=`budget_exceeded`），`context_manifest_items.included=false` 留 source_id；
- 老 tail 中的 T02 + T03 开头段被压缩成 L1a（保结构化事实 + 删对话语气），释放 ~400 token；
- tool_schemas drop 了 8 个 edge-only schema（`reason='workspace_unavailable'`，G1），释放 1000 token；
- plan_todo drop 了 9 条 done/pending（`reason='plan_tree_sibling_compression'`）。

**观察**：
1. `tool_schemas=700` 是 cloud 的硬底线（7 个最小工具各 100 token），再砍就不够描述 `sandbox.fs.write` 的必要参数；
2. `recent_tail 2600` 本来是 `budget_v1_8k` "可临时扩 2800"（L2420）—— 这里没到 2800 是因为 retrieved 拉满 1000；
3. `retrieved=1000` **恰好**够装 3 candidate + 差异句，**5 candidate 情况下不够**（F-6）；
4. 若此时发起 delegation（G18 cap 1500），需要从 tail 砍 1000 + retrieved 砍 500 → tail=1600（踩 floor）/ retrieved=500（candidate 差异句都得删）→ 组合 1 风险暴露。

**T10 对比推演**（progressive_loading 最紧张）：

T10 要求"同时看 4 块内容：方案 A 原文 680 + eslint preview 320 + background.js 贴码 520 + storage skeleton 600 = 2120 token"，但 retrieved(1000) + tool_previews(500) + tail 的 artifact 槽位合计只 1500。

| Zone | 期望 | 实际落盘 | 差值处理 |
| --- | ---: | ---: | --- |
| retrieved (A 方案原文) | 680 | **420**（compressed_summary 再压一档） | drop 细节例子，留 `source_hash` |
| tool_previews (eslint+screenshot) | 320+120=440 | **440** | eslint preview 按 G8 `eslint_v1` 模板首屏 |
| tail 中 background.js 贴码 | 520 | **140**（artifact_ref + 一句"47 行 JS，监听 `chrome.runtime.onMessage`"） | 原 520 token 转 `artifact_ref`，preview_text=140 |
| tail 中 storage skeleton | 600 | **360**（保留 interface 签名，删 body） | `render_mode='compressed_summary'` |

核心策略：第一轮 manifest 只进必要 signatures/refs，**下一轮用户真要看细节时**，context builder 下一轮把 retrieved 临时扩到 1500 + 砍 tail 500 来读回 artifact —— 这就是 S14 §4 的"递进式加载"。v0.2 G1 manifest reason 缺 `progressive_loading` 枚举是 F-2 / 候选 G21。

**T10 总计**：retrieved 420 + tool_previews 440 + tail(含 2 artifact slot) 1960 + anchor 200 + plan_todo 380 + summary 490 + workspace 100 + system 1700 + tool_schemas 1700(edge 15) = **7390 token**（刚好 ≤7492），但**只塞了 1500 tokens 可用于展示 4 块内容中的 signature**，实际信息只有原需求的 ~70%；余下 30% 走下一轮 artifact fetch。这是 8k 窗口下可接受的**可控降级**。

---

## §8 最终判断

- **本场景下 v0.2 是否可实现**: ✅（有 notable friction，但无 blocking）
- **顺畅度**: ⚠️ 有明显摩擦 —— T05 / T09 两次歧义 ask 连环触发 + F-3 用户体验心理负担 + F-5 首轮 probe 时机未规定 + F-6 retrieved 1000 对 5 候选不够。小姚会感觉 "AI 好像很诚实但很啰嗦"。
- **一句话总结**：**v0.2 的 G10+G11+G12+G13 四块拼图在 8k × 4 设备 × 歧义三压叠加下能跑起来，但 small-model JSON 兜底（新 G20）、retrieved × max-5-candidates 的预算冲突（F-6/组合 3）、首轮 probe 时机（F-5）三处须在 Phase 3 实现前补字；真正的隐患不是"设计不可达"，而是"设计留白导致各实现分叉"**。
