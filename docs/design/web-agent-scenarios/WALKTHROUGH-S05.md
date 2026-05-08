# S05 Walkthrough — PM 一天内改 8 次「积分商城」需求

> Scenario: S05 plan-thrashing（DAUT: A6 / A11 / A15）
> Runs against v0.2 design (web-agent-session-state.md, 3040 lines, G1–G19 resolved)
> Author stance: 有经验的工程师，按 v0.2 真的实现并跑一遍

## §1 场景标识与 v0.2 映射

**一句话场景**：小王（电商 PM）周四 09:00–19:30 在同一个 session 里把「会员积分商城」需求来回改 8 次（兑换 → +折扣 → +抽奖 → +拼团 → +分享+排行榜 → -排行榜 → 砍 MVP → +签到+邀请），次日 09:00 回话说"继续…回到上午那版"。被测 DAUT：A6（decision 不可物理删）、A11（plan_todo ≤ 800 tokens 硬上限）、A15（next-action 分级处理歧义）。

**关键 v0.2 条款**：

- **G2 Compaction Invariants** @ L2496–2596：#2 `plan_state/decision/active todo_state` 不可 replace/archive；**#3 `plan_state.version` 在 compaction 轮次禁止 bump**；#8 summary 只追加或 archive 不可 delete。8 条 assertion 都要 return 0（除 manifest 计数）。
- **G6 Cross-Session Scope** @ L2302–2353：`scope='user'` 白名单（durable_decision / engineering_rule / rejected_pattern）；backlog 用 `session_todos.backlog_pool_id` + `status='backlog'`（L2209 / L2346）；新 session 按 `(user_id, scope='user')` 加载 ≤400 token 进 anchor zone。
- **G12 Next-Action Confidence State Machine** @ L657–698：`≥0.8` 自动 accept / `0.5–0.8` ask_user / `<0.5` 直说没看懂；优先级 structured_event → rule → small_model（L641–651）；单轮最多 5 条 suggestion、不同 source 可并存不可覆盖。
- **G14 mutation 枚举** @ L1154–1215：`{insert, update, replace, archive, delete, bubble_up, apply_suggestion, activate}`；`apply_suggestion` payload 关联 `suggested_next_action.id`；`status='backlog'` 非 compaction 候选（L1211–1213）。
- **Plans and Todos schema** @ L2196–2240：`session_todos` 已含 `backlog_pool_id`、`origin_session_id`、`idx_todos_backlog_pool`；`session_todo_deps` 只存边，图遍历靠 status 过滤。
- **session_state_item_events schema** @ L1106–1126：`previous_version` / `next_version` 列存在，plan_state 的版本回放走 audit 链而非 payload JSON。

## §2 运行前提

- **Schema 初值**：`agent_sessions{session_id=sess_wangpm_d1, user_id=u_wang, project_id=proj_mall, workspace_id=ws_cloud, scope_defaults='session'}`；`agent_runs` 未启动，T1 触发首个 `run_t1`。
- **User memory（scope='user'）**：小王既有 2 条 `engineering_rule`（"合规先于 GMV"、"MVP 先跑通，再谈社交"）和 1 条 `rejected_pattern`（"抽奖/开箱类需文化监管许可"）。T1 context 初始化按 L2328–2339 查询，≤400 tokens 落到 `session_anchor` zone。**关键**：这些 rule 在 T4 合规否决抽奖时会被 agent 主动引用。
- **Workspace**：cloud-only（PM 场景无 edge），工具集剪裁掉代码/shell，仅保留 `render_prd` / `search_kb` / `write_todo` / `save_artifact` 等 PRD 协作工具，`tool_schemas ≈ 1400` tokens。
- **Budget template**：默认 `budget_v1_32k`（非 S14 的 8k），因此 `plan_todo cap=800` / `recent_tail` 可扩到 ~3000 / `retrieved_facts cap=1500`。manifest 需记录 `tokenizer_id='o200k_base'` + `budget_template_id='budget_v1_32k'`（L2436）。
- **Backlog pool**：`backlog_pool_id=pool_mall_v1`，user-scoped，T7 老板砍 MVP 时预留用来收容被砍的拼团/排行榜等。
- **NextActionExtractor 配置**：按 L642–651 优先级（structured → rule → small_model），confidence 阈值按 L667–671 三档。
- **预期 context window**：默认 32k，T4/T9 compaction 前 `recent_tail` 已累计 ~9.6k/~5k，仍在预算内但接近 zone 上限触发压缩。

## §3 端到端运行轨迹（12 回合）

### T1 · 09:00 开局「积分兑换页面」

- **User**: "我们要给 APP 加个会员积分商城，第一步做积分兑换页面。你帮我拟个 plan 和初版 todo，别太重，我要先跟老板过方向。"
- **Server**: 新建 `run_t1`。`ContextBuilder` 按 G6 L2330 查 `scope='user'` 共 3 条 rule/pattern → `session_anchor` 300 tokens。`write_todo` 工具批量 insert 6 条 todo（`mutation='insert'`，source='agent'），`session_todos.status='active'`；`session_todo_deps` insert 3 条（兑换流程 ← 详情 ← 列表、下单幂等 ← 兑换流程）。`session_state_items` upsert `plan_state{version=1, phase='draft_v1_积分兑换'}` 写 event(mutation='insert', previous_version=NULL, next_version=1)，`decision{key='product.goal.v1'}` 写 event(mutation='insert')。写 1 条 `context_manifests(reason='initial_turn', tokenizer_id='o200k_base')`。
- **User sees**: SSE `plan.updated` + `todo.batch_inserted` × 6 + `decision.upserted`；UI 显示 v1 plan 卡片 + 6 条 todo + 1 条 decision。manifest zone 估算：system 1200 / tools 1400 / anchor 300 / plan_todo 480（6 active × ~80 + deps） / summary 0 / tail 0。总 ≈ 3380，dropped=0。

### T2 · 10:30「加多级会员折扣」

- **User**: "老板过完——他要加多级会员折扣，银/金/钻三档，等级越高折扣越狠，冲 GMV。"
- **Server**: `plan_state` upsert（mutation='update'，previous_version=1，next_version=2，phase='draft_v2_兑换+折扣'）。4 条新 todo insert（active）；deps 新增 4 条。`decision{key='product.tier_discount.added', status='tentative', priority='medium'}` insert。此刻 agent **应** 按 G6 把 user-scope `engineering_rule="合规先于 GMV"` 作为弱警示写进 response，但未主动 `ask_user`（老板只是"要加"，不是违规）。
- **User sees**: 10 条 active todo 卡片；plan v2。manifest: anchor 300 / plan_todo 680（10 × ~65 + deps） / tail 1200（T1 + T2 前半）/ retrieved 120（3 条 user-scope rule 前排）/ summary 0。总 ≈ 4300。`reason='plan_revision'`，dropped=0。

### T3 · 11:45「折扣砍掉，加抽奖」

- **User**: "等下——财务说多级折扣要重做积分账本成本太高，UI 位又紧。砍掉。运营想加积分抽奖拉日活。"
- **Server**: `plan_state` bump（prev=2,next=3,phase='draft_v3_兑换+抽奖'）。4 条折扣 todo `UPDATE status='cancelled'` → event(mutation='**update**'，payload_json 里标 `status_change='active→cancelled'`，G14 L1156–1164 mutation 枚举没有 `cancel` 值，**必须走 update 改 status 列，不能走 archive**：archive 按 L2516 只给 completed）。`session_todo_deps` 里 4 条边保留行但渲染时按 active-only 过滤（schema 本身无 `valid` 列，context builder 用 `JOIN session_todos ON status IN ('active','in_progress')`）。新增 3 条抽奖 todo。关键动作：`decision{key='product.tier_discount.added'}` mutation='**replace**'（前行 status='superseded'），新 `decision{key='product.tier_discount.rejected', status='active', payload.rationale='cost=会员账本重做+UI=与兑换价位互斥'}` insert。
- **User sees**: 折扣卡片收到折叠区标"已取消"；3 条抽奖 todo 新增；retrieved_facts 开始吸收 1 条 rejected decision。plan_todo 720（6+3 active × ~65 + deps） / retrieved 300（user rules 120 + 1 decision 180） / tail 1400 / summary 0。总 ≈ 4580，`reason='plan_revision'`，dropped_count=4（cancelled）。**LLM 看不到**: 4 条 cancelled 折扣 todo 的完整 description（~1100 tokens）留 DB。

### T4 · 13:00「抽奖合规不行，改拼团」（首次 compaction 内嵌触发）

- **User**: "中午合规找我——抽奖归文化监管要许可证，App Store 也卡。抽奖砍掉。改成拼团：成团价低、不成团退积分。"
- **Server**: 先做需求改动：`plan_state` bump(3→4,phase='draft_v4_兑换+拼团')；3 条抽奖 todo → `status='cancelled'`（update）；5 条拼团 todo insert（开团发起/团详情/凑团分享/成团结算/退积分兜底）；deps 重建 3 条。`decision{key='product.lottery.rejected', status='active', rationale='compliance=文化监管许可+appstore_review_risk'}` insert，引用 user-scope `rejected_pattern`（origin_state_item_id 串链）。**然后** CompactionPolicy 观察到 `recent_tail` ≈ 9600 tokens + plan version=4，触发 **inline compaction 子事件**：按 G2 invariant #4 先断言当前 run `running`，**失败** → 只能把 T1–T3 的 transcript 压到新 summary item（L1b，key='summary:morning_revisions_v1'，tokens≈380），**不动** plan_state.version（保持 4，G2 invariant #3）。
- **User sees**: 拼团卡片 5 条；T1–T3 transcript 折叠成「上午 3 次反转摘要」。plan_todo 740（6+5=11 active 逼近 800） / summary 380 / retrieved_facts 360（user rules 120 + 2 rejected decisions 240） / tail 700 / anchor 300。总 ≈ 4880。manifest `reason='post_compaction'`，dropped=3（cancelled 抽奖）。**注意**：场景文 §3-T4 写"CompactionPolicy 在 recent_tail 累计阈值触发" → 合规的；但若 inline compaction 和 plan revision 同属一个 turn，invariant #4 的 "agent_runs.status NOT IN ('running','waiting')" 会告警 → **必须把 compaction 延后到 turn 结束、run 关闭后再做**。这是个真 friction（见 §4-F1）。

### T5 · 14:30「拼团 + 分享攒积分 + 排行榜」

- **User**: "拼团老板认可了——又加码：让用户分享链接攒积分，搞分享积分排行榜，top 100 发实物奖品，做成小型社交事件。"
- **Server**: `plan_state` bump(4→5,phase='draft_v5_兑换+拼团+分享+排行榜')。6 条新 todo insert（分享链接生成 / 分享积分结算 / 分享防刷 / 排行榜页 / 每日榜 job / 奖品发放）；deps 6 条。`decision{key='product.social_expansion.added', status='tentative', priority='medium', rationale='老板指令，待成本评估'}` insert。
- **User sees**: 17 条 active todo 撞到 `plan_todo cap=800`（17 × ~50 = 850 > 800） → `ContextAssembler` 按 A11 对 3 条最低 P2 项做 `title-only fallback`（去掉 description 行），压到 790。manifest: plan_todo 790 / summary 380 / retrieved 360 / tail 1300 / anchor 300。总 ≈ 4730，`reason='plan_revision'`，dropped=0（但 plan_todo 渲染器已进入 title-only 降级）。`context_manifest_items` 为 3 条 title-only todo 各写一行 `render_mode='title_only'`。

### T6 · 15:45「排行榜砍，保留好友助力」

- **User**: "排行榜内部讨论过，激化对立 + 客服接不住榜位纠纷。砍掉。但好友助力留下——好友点一下帮攒积分。"
- **Server**: `plan_state` bump(5→6,phase='draft_v6_兑换+拼团+分享+好友助力')。3 条排行榜 todo → `status='cancelled'`（update）；2 条好友助力 todo insert（助力按钮/H5 落地 / 助力防刷+上限）；deps 重建（-3 +1 = 净 +1 有效边，被砍边逻辑过滤）。`decision{key='product.leaderboard.rejected', status='active', rationale='激化用户对立+客服承接不了榜位纠纷'}` insert。累计 active decision 数：goal.v1 + tier_discount.rejected + lottery.rejected + leaderboard.rejected + social_expansion.added(tentative) = 5。
- **User sees**: plan_todo 770（17-3+2=16 active）/ summary 380 / retrieved 540（3 rejected decisions 450 + user rules 90） / tail 1300 / anchor 300。总 ≈ 4860，`reason='plan_revision'`，dropped=3。

### T7 · 17:00「老板砍 MVP，只留积分 + 兑换」

- **User**: "完了完了——老板开会说 Q3 融资路演不能背这么重的包，让我只做积分+兑换，拼团/分享/好友助力全砍。"
- **Server**: `plan_state` bump(6→7,phase='mvp_v1_纯积分兑换')。**批量** 10 条 todo（拼团 5 + 分享 3 + 好友助力 2）→ status='cancelled'（update）。关键动作：小王后续第二天还想"捡回"这些，server **应当** 同时把 10 条写入 `backlog_pool_id='pool_mall_v1'` 并改 `status='backlog'`（G6 L2346 + G14 L1211–1213 明确 backlog 是第三态），但 **v0.2 没说 cancel → backlog 的自动转移策略**——是 T7 此刻直接写 backlog，还是 T12 收尾时才写？见 §5 新 gap G20。本回合采取保守策略：10 条先 cancelled，等 T12 延迟 promote。`decision{key='product.mvp_cut_by_boss', status='active', priority='critical', rationale='Q3 融资路演，老板指令砍到只保留积分+兑换'}` insert。`social_expansion.added` 前行 mutation='replace'（status→'superseded'）。active todo 回到最初 6 条。
- **User sees**: plan 卡片大面积"已取消"折叠；plan_todo 460（6 active × ~70）大幅收窄 / summary 380 / retrieved 640（5 active decision，critical 置顶） / tail 1400 / anchor 320（加入"MVP 决策已由老板拍板"）。总 ≈ 4400，`reason='plan_scope_cut'`，dropped_count=10。

### T8 · 19:30「加签到和邀请拿积分」（第 8 次，token 账要算清）

- **User**: "回家路上想了下——MVP 里积分只有兑换这一个出口，'怎么赚积分'路径太单薄。加两个轻量入口：每日签到 + 邀请新人注册。不涉合规、不涉大开发量。"
- **Server**: `plan_state` bump(7→8,phase='mvp_v2_积分+兑换+签到+邀请')。4 条新 todo insert（每日签到页 / 签到积分结算 / 邀请链接生成 / 邀请注册回调结算）；deps 4 条（签到结算 ← 签到页、邀请结算 ← 邀请链接 等）。`decision{key='product.earn_path.added', status='active', rationale='签到+邀请作为积分获取入口，合规低风险，未突破 user rule "合规先于 GMV"'}` insert。累计 `session_state_item_events` 条数：T1(8) + T2(5) + T3(7) + T4(11+compaction 额外 3) + T5(7) + T6(5) + T7(12) + T8(5) ≈ 63 条，audit 链完整。
- **User sees**: plan_todo 620（10 active × ~62） / summary 380 / retrieved 720（6 active decision + 1 tentative + user rules 的 top-K） / tail 1400 / anchor 320。总 ≈ 4740。详见 §7 token 账。

### T9 · 20:00 第二次 compaction（dedicated，非 turn）

- **User**: （无；CompactionPolicy 定时/阈值触发，小王下班前瞄一眼进度）。
- **Server**: 按 G2 invariant #4 先确认 `run_t8` 已 `status='completed'` → 允许 session-level compaction。新写 `summary{key='summary:day_one_converged_v1', status='active', tokens≈520}` 叙述"8 次反转，最终 MVP=积分+兑换+签到+邀请，4 方向被否"。旧 summary `morning_revisions_v1` mutation='archive'（payload 保留，L2523–2524）。**关键合规验证**：按 G2 invariant #3，这轮不许 bump `plan_state.version`——**场景原文 §3-T9 写"version=9"是错的**，v0.2 下 T9 之后 plan_state.version **依然是 8**。版本 9 只能由 T11/T12 的真实执行产生。 assertions：forbidden_state_mutations=0、forbidden_plan_version_bumps=0、active_runs=0、post_compaction_manifest_count=1，全 pass。
- **User sees**: UI 显示"会话已归档上午部分"。manifest zone（纯 compaction manifest，非 turn prompt）：summary 520（新）+ 380（archived，不进 prompt）/ plan_todo 620（未变）/ retrieved 720 / anchor 320 / tail 压到 800。总 ≈ 4220，`reason='post_compaction'`，tokenizer_id='o200k_base'，dropped=0。**LLM 看不到**：T1–T6 的原始对话（~12k tokens），旧 summary 归档但不进 prompt，20 条 cancelled todo 的完整 description。

### T10 · 次日 09:00「继续」/「回到上午那版」（A15 歧义测试）

- **User**: "（打开 session）继续吧。……等等不对不对，回到上午那版。"
- **Server**: **第一步**——"继续"命中 NextActionExtractor step 1 structured_event：当前 `plan_state` (version=8, mvp_v2) 直接作为续接对象，生成 `suggested_next_action(id=sna_1, source='structured_event', confidence=1.0, status='accepted', payload.ref='plan_state@v8')`。**第二步**——"回到上午那版" 命中 step 2 rule extractor："回到 X 那版" 模式匹配成功 → 意图=回溯；指代"上午那版" 候选 = {v1 纯兑换 / v2 +折扣 / v3 +抽奖 / v4 +拼团 / v5 +社交加码}（v6 已是下午 15:45 不算上午），候选数 5 → confidence≈0.55（落 0.5–0.8 档，L669）→ **不自动 rollback plan_state**，写 `suggested_next_action(id=sna_2, source='rule', confidence=0.55, status='pending', expires_at=+1h, payload.candidates=['v1','v2','v3','v4','v5'])` 并发起 `ask_user` 澄清。按 G12 L686 单 turn 最多 5 条 suggestion → OK。**两条并存** sna_1 和 sna_2，v0.2 L661–663 明确"不同 source 的 suggestion 可共存，低置信不覆盖高置信"。
- **User sees**: UI 同时显示「✅ 已接上 MVP v8 (积分+兑换+签到+邀请)」和「❓ '上午那版' 指哪一版？请选择：(1) 09:00 纯兑换 (2) 10:30 +折扣 (3) 11:45 +抽奖 (4) 13:00 +拼团 (5) 14:30 +社交」。manifest: plan_todo 620（不动） / **suggested_next_action 新 zone 220**（2 条 suggestion 含 candidates） / summary 520 / retrieved 720（6 active decision + 5 plan phase title 供对照） / tail 400 / anchor 320。总 ≈ 4800，`reason='ambiguity_clarification'` 或 `continue_with_ambiguity`（**reason 枚举 L2436 是否包含？见 §5 gap 候选**），dropped=0。

### T11 · 09:30 小王确认后 agent 执行 MVP

- **User**: "算了不回滚了——就按昨晚那版（积分+兑换+签到+邀请）推进。先把 PRD 草稿出来，tech lead 要看。"
- **Server**: sna_2（低置信歧义）mutation='update'，status='resolved_by_user_choice', expires_at=now（主动过期）。sna_1（高置信已 accepted）写 event `mutation='apply_suggestion'`（G14 L1181–1190）payload={suggested_next_action_id='sna_1', chosen_candidate_id='cand_a', source='user_explicit', confidence_at_accept=1.0}。**plan_state 首次真正 bump**: version=8→9（phase 不变，但 `payload_json.execution_started_at=now` → 语义变化合法 bump）。「PRD 文档骨架」todo → status='in_progress'。agent 调 `render_prd` → 48KB artifact 落 `session_artifacts{ref='artifact:prd-integral-mall-mvp-v1', byte_size=49152}`，preview 500 char 进 `tool_previews` zone。
- **User sees**: PRD 预览卡片 + "生成中"进度；plan_todo 640（10 active，PRD 那条高亮 in_progress） / summary 520 / retrieved 720（6 decision 全留，PRD 要在"遗留风险"章节引用） / tool_previews 180 / tail 700 / anchor 320。总 ≈ 4520，`reason='execute_after_clarification'`（**新 reason，L2436 枚举也未必覆盖**）。

### T12 · 11:00 交付小结

- **User**: "PRD 给 tech lead 看过了，他说 OK。帮我写今天的 decision 清单贴周报，要能看出为什么砍、为什么加。"
- **Server**: 纯 structured query（无 LLM 推理）：`SELECT * FROM session_state_items WHERE session_id=? AND category='decision' AND status IN ('active','superseded') ORDER BY priority DESC, created_at ASC`。一次拿 7 条 decision（5 active + 2 superseded）。再一次 `SELECT previous_version, next_version, payload_json FROM session_state_item_events WHERE session_id=? AND category='plan_state' ORDER BY id ASC` 拿 v1→v9 的 9 版 phase 迁移。渲染为 artifact `decision-log-day1`。**关键 backlog promote**：T7 被 cancelled 的 10 条（拼团/分享/好友助力）+ T6 的 3 条排行榜 + T3/T4 的折扣/抽奖 7 条 = 共 20 条，按小王指示 "follow-up backlog" 批量 `UPDATE session_todos SET status='backlog', backlog_pool_id='pool_mall_v1', origin_session_id='sess_wangpm_d1'`（G6 L2346）。这 20 条从此对新 session 可见（L2229 idx_todos_backlog_pool）。anchor 新增子项 `day_one_delivered`。
- **User sees**: 交付报告预览；manifest 破例扩：retrieved_facts 1800（7 decision + 9 plan phase） / plan_todo 260（仅剩交付确认） / summary 520 / anchor 400。总 ≈ 5000，`reason='final_delivery_summary'`，tokenizer_id 记录。

## §4 发现的别扭点（Frictions）

### F1 · T4 inline compaction 与 active run 的顺序冲突 — **notable**

- **Ref**: T4；v0.2 §Compaction Invariants L2513–2515（invariant #4）
- **Observation**: 场景 T4 要求"需求改动 + inline compaction"在同一个 turn 完成（合规砍抽奖、加 5 条拼团、同时触发 compaction 把 T1–T3 压成 summary）。但 G2 invariant #4 assertion "active_runs=0" 必须在 compaction 前 pass，而 turn 内 `run_t4` 必定 `status IN ('running','waiting')`。
- **Why it's friction**: 实现时 CompactionPolicy 只能把 compaction job 延迟到 run 结束后的 post-turn hook 跑。对用户体验的影响是：T5 进来时 recent_tail 还没压下去，plan_todo + tail 合计 ≈ 10k，逼近 32k 窗口并不致命；但对实现者来说，"边聊边压"的想法在 v0.2 下不合法。
- **Severity**: notable（不 block，但推翻了场景原文的时间线写法，要改"inline compaction" → "post-turn compaction"）。

### F2 · plan_state.version 在 compaction 轮次禁止 bump（场景 §3-T9 原文写错）— **notable**

- **Ref**: T9；v0.2 §Compaction Invariants L2511 + L2543–2551
- **Observation**: S05 场景原文 §3-T9 写"plan_state version=9（compaction 带来的结构化重排）"，但 v0.2 invariant #3 明确禁止 compaction turn bump plan version。真正跑时 T9 结束 plan_state 仍然是 v8，只有 T11（用户确认执行）或 T12（交付收尾）才能合法 bump 到 v9。场景原文预计 "9 次 bump"，实际只能有 8 次 bump（T1–T8 每轮 +1）+ T11/T12 的 1 次合法 bump = **9 次 total 正好，但 bump 归属不同**：场景误把 compaction 算作 bump。
- **Why it's friction**: 对 audit/回放 API 的影响：如果实现者按场景原文写，T9 bump 会触发 `forbidden_plan_version_bumps > 0` 断言失败，整个 compaction 回滚。
- **Severity**: notable（v0.2 覆盖但场景原文与设计错位；建议 S05.md 自身 §3-T9 措辞修正）。

### F3 · G14 mutation 枚举不含 `cancel`，todo 从 active → cancelled 走哪个值？— **notable**

- **Ref**: T3 / T4 / T6 / T7（每次 cancel todo）；v0.2 §G14 L1156–1164
- **Observation**: G14 列 mutation = {insert, update, replace, archive, delete, bubble_up, apply_suggestion, activate}，无 `cancel`。todo status 从 `active` 改成 `cancelled` 只能走 `update`（修改 status 列）。但 `archive` 按 L2516 "Completed subtasks may be archived"，`cancelled` 不是 completed——档位上 archive 不适用。实现者可能犹豫：是 update 还是 archive？
- **Why it's friction**: 审计查询 "这条 todo 什么时候被砍的？"用 `SELECT * FROM session_state_item_events WHERE mutation='update' AND payload_json->>'status'='cancelled'`，比 `WHERE mutation='cancel'` 或 `WHERE mutation='archive'` 多一层 JSON 过滤，性能与可读性都差。S05 一天 cancel 20 条 todo，这个查询会跑 20 次。
- **Severity**: notable。见 §5 gap 候选 G20。

### F4 · session_todo_deps 没有 `valid` 列，cancel todo 的边靠 JOIN 过滤 — **minor**

- **Ref**: T3 / T4 / T6 / T7 重建依赖图；v0.2 §Plans and Todos L2232–2240
- **Observation**: 场景原文假设 "deps 行打 `valid=false`"，但实际 schema 只有 `{todo_id, depends_on, session_id, created_at}`，没有 `valid` 列。图重建靠 `JOIN session_todos ON status NOT IN ('cancelled','archived')`。一天 8 次需求反转累计 deps 行数 ~45，JOIN 性能 OK，但"被砍的边要不要 DELETE 行"有歧义：场景希望保留（审计），v0.2 schema 没明说。
- **Why it's friction**: 实现者可能错误地 `DELETE FROM session_todo_deps WHERE todo_id IN (cancelled_ids)` 以简化图遍历，丢失审计。
- **Severity**: minor（建议文档注 "deps 行不 DELETE，图遍历靠 JOIN active todo"）。

### F5 · T7 cancel 与 T12 backlog promote 的时机/自动化未契约化 — **notable**

- **Ref**: T7 被砍 10 条 + T6 被砍 3 条排行榜 + T3/T4 砍的 7 条；T12 批量 promote 到 `backlog_pool_id='pool_mall_v1'`
- **Observation**: G6 L2346 规定 "Backlog todos that should survive a session reset use `backlog_pool_id` and `status='backlog'`"，但没说"被 cancelled 的 todo 什么时候变 backlog"。场景让小王 T12 显式指示 "follow-up backlog" 才 promote，但实务上用户很可能忘记说——那 20 条 cancelled todo 就永远留在单 session 里，**下次新 session 想捡不到**，B6 的跨 session 价值失效。
- **Why it's friction**: 实现者需要一个策略：是每次 cancel 就 offer "要不要 promote 到 backlog？"（打扰用户），还是 session 结束时批量扫 cancelled 问一次（更合理），还是每次 cancel 默认写 backlog_pool_id 但保留 status='cancelled'（等未来 promote）？v0.2 都没定。
- **Severity**: notable。见 §5 gap 候选 G21。

### F6 · T10 两条 suggestion 并存的 UI 表达 — **minor**

- **Ref**: T10；v0.2 §G12 L661–663
- **Observation**: G12 L661 "Suggestions from different sources coexist; lower-confidence candidates must not overwrite"，但 UI 怎么渲染"同时出现一条 accepted + 一条 pending"没规定。小王看到两张卡片可能懵："到底 agent 准备做哪个？"v0.2 只给了 state machine，没给 UI render contract。
- **Why it's friction**: 前端可能把 sna_1（accepted）渲染成"下一步：继续做 MVP v8"，把 sna_2（pending）渲染成"你说'上午那版'指哪版？"但用户读起来像两个矛盾指令。
- **Severity**: minor（建议 UI 层单独出 render spec，把 accepted 放"已确认"区、pending 放"需澄清"区）。

### F7 · plan_todo cap=800 在 T5 触发 title-only fallback 的结构缺失 — **minor**

- **Ref**: T5（17 条 active 撞 800）；v0.2 §Budget Policy
- **Observation**: A11 指定 plan_todo cap=800，但 title-only fallback 的具体渲染规则（哪些字段保留、哪些丢）v0.2 没给。场景原文说 "agent 自动把 3 条最低 P2 细节项折叠成 title-only"，但"最低 P2"怎么挑——按 priority？按 depth？按 created_at？不确定。
- **Why it's friction**: 实现者可能按不同策略挑，导致跨环境行为不一致；且 `context_manifest_items.render_mode` 是否支持 'title_only' 枚举也未明确（schema 里有 `render_mode` 列吗？需确认）。
- **Severity**: minor（对正确性无影响，对可观测性有影响）。

## §5 发现的新 gap 候选

### G20. `session_state_item_events.mutation` 缺 `cancel` 值（或明确 cancel → update 的子语义）

- **title**: todo status active→cancelled 的 mutation 归属未契约化
- **triggered_by_turn**: T3 / T4 / T6 / T7（4 次批量 cancel）
- **related_v0.2_section**: §Session State Projection G14 L1156–1164；§Compaction Invariants L2516（archive 只给 completed）
- **severity**: medium
- **rationale**: G14 枚举 {insert, update, replace, archive, delete, bubble_up, apply_suggestion, activate} 没有 `cancel`，而 `archive` 按 v0.2 语义只给 completed subtask。todo 从 active 变成 cancelled **逻辑上是结构性变更**（进 dropped_count、影响 deps 图、进 retrieved_facts 排序），用 `update` 写 event 则需 payload_json 里加 `status_change` 字段做语义解析，审计查询要 JSON 过滤。S05 一天 cancel 20 条，40 次查询放大效应明显。
- **proposed_fix**: 两选一：(a) 枚举加 `cancel` 值，payload 含 `{previous_status, cancel_reason, cancelled_by}`；(b) 明文规定 "status transition 走 update，payload_json 必含 `transition:{from,to}` 字段"，并给 SQL 范例。推荐 (a)。

### G21. cancelled todo → backlog_pool 的 promote 策略缺

- **title**: `session_todos` cancelled 与 backlog 之间的转换时机/自动化未规定
- **triggered_by_turn**: T7（10 条批量 cancelled）+ T12（批量 promote 到 pool_mall_v1）
- **related_v0.2_section**: §Cross-Session Scope and User Memory L2345–2347；§G14 L1211–1213
- **severity**: medium
- **rationale**: G6 定义了 backlog_pool_id 与 status='backlog' 的存在，但没定义"cancel 的 todo 什么时候/如何 promote"。S05 T7 砍 10 条、T12 用户显式说 "follow-up backlog" 才 promote——但用户很可能忘记。如果没有默认策略或引导式 UI，B6 的跨 session 复用价值会在"用户没想起来说 backlog"时失效。
- **proposed_fix**: 引入 `cancel_reason ENUM('scope_cut','rejected','dedup','superseded')` 列（schema change）；对 `scope_cut`（老板砍 MVP 类）默认写 `backlog_pool_id` 但保留 `status='cancelled'`，session 关闭时弹一次确认 "要把 12 条 scope-cut todo 转为 post-MVP backlog 吗？"。另一方案：session 结束 hook 里自动扫最近 1h 内的 cancelled + scope_cut 批量 promote 并通过 event 通知用户。

### G22. `context_manifests.reason` 枚举未覆盖 ambiguity/clarification 场景

- **title**: T10 歧义澄清 turn 的 reason 值无合适枚举
- **triggered_by_turn**: T10（continue + ambiguous rollback 同 turn）
- **related_v0.2_section**: §G1 Context Manifest Reason Enum（G1 已 resolved, 7 lifecycle/compaction/retrieval 类）
- **severity**: low
- **rationale**: G1 枚举含 `initial_turn / post_compaction / history_recall_{structured|fts|vector} / large_tool_output_gated / plan_subtree_query / tree_structured_report / workspace_switch / approval_resume / cross_session_recall`，没有对应"澄清 / 回溯请求 / 多 suggestion 并存"的 reason。S05 T10 实际 reason 属于 "next_action_extraction_ambiguous"，落不到现有枚举，实现者会乱写字符串，污染 G1 的可聚合性。
- **proposed_fix**: 补两个 reason: `next_action_ambiguous`（低置信回溯/歧义） + `execute_after_clarification`（用户 apply_suggestion 后首个执行 turn）。

## §6 Gap 组合效应检验

### 组合 1: G2 + G14 + G6（compaction × mutation × backlog pool）

- **运行时可能发生的坑**: T9 compaction 发现 T7 批量 cancelled 的 10 条 todo。按 G2 invariant #2，active todo_state 不可 replace/archive；cancelled 状态的 todo 不在保护范围，但如果 compaction 想"顺手"把它们 promote 到 backlog_pool（G6 L2346）节省 DB 扫描，就要写 `mutation='update'`（改 status='backlog' + backlog_pool_id）。**但** G14 L1211 "Backlog todos are not compaction candidates" 明确禁止 compactor 改 backlog 行——问题是 cancelled → backlog 的这一步 transition 是"被 compaction 触发"还是"独立 job"？歧义。
- **v0.2 是否覆盖**: partial — G2 保护 active，G14 保护 backlog，但 cancelled → backlog 的 transition 归属（turn-driven / compaction-driven / session-close-driven）没写。
- **如果 no，建议**: 明确 promote 是 turn-driven（由用户 apply_suggestion 或 session-close hook 触发），**compaction 永远只改 summary，不碰 todo status**。在 §Compaction Invariants 里再加一条 invariant #9：`compaction turn 不允许对 session_todos 写任何 event`。

### 组合 2: G12 + G14（next-action suggestion × apply_suggestion mutation）

- **运行时可能发生的坑**: T10 生成 2 条 suggestion（sna_1 accepted, sna_2 pending），T11 用户选择 "不回滚" → sna_2 变成 resolved_by_user_choice。G14 L1181–1190 `apply_suggestion` payload 规定 `suggested_next_action_id + chosen_candidate_id + source + confidence_at_accept`，但**对 "rejected / resolved_without_choice" 场景没有对应 mutation**——sna_2 用户没选任何候选，是走 `update`（status 改）还是 `apply_suggestion` 配一个 `chosen_candidate_id=null`？v0.2 要么没写、要么写得隐晦。
- **v0.2 是否覆盖**: partial — apply_suggestion 的 "apply" 语义偏向"接受了"，用户"拒绝了/放弃了"这条歧义的 suggestion 怎么写 event 不够明确。
- **如果 no，建议**: G14 payload 允许 `chosen_candidate_id=null` + 新字段 `resolution='accepted'|'rejected'|'expired'|'superseded_by_newer_suggestion'`，或在 mutation 里加 `dismiss_suggestion` 值。

### 组合 3: G6 + G12 + G2（user-scope rule × next-action confidence × compaction 保护）

- **运行时可能发生的坑**: T1 user-scope `engineering_rule="合规先于 GMV"` 加载进 anchor。T2 老板要折扣（纯 GMV 导向），**agent 是否应该自动把这条 rule 作为"弱反对"suggestion？** 按 G12 rule extractor 应该能命中，但 confidence 多少？结构化匹配是 1.0，但语义上"user rule vs 老板指令"有 priority 冲突，g12 没给这种"user-rule-vs-current-request"的冲突解决流程。再叠加 G2：即使 agent 写了这条 suggestion，compaction 时不能改 user-scope state item（invariant #2 保护 decision，但没明说保护 durable_decision / engineering_rule 这些 user-scope 行），理论上 compactor 可能误伤。
- **v0.2 是否覆盖**: partial — G2 invariant #2 列了 category IN ('plan_state','decision',...)，**但没列 'durable_decision'/'engineering_rule'/'rejected_pattern'**（这些是 G6 新加的 scope='user' 专用 category）。S05 T4 合规砍抽奖时 agent 确实需要引用 rejected_pattern="抽奖需许可证"，如果哪次 compaction 把这条 archived 了就糟了。
- **如果 no，建议**: G2 invariant #2 的 category 白名单扩到包含 `durable_decision / engineering_rule / rejected_pattern`（即使它们是 scope='user'，在 session 内依然必须保护）。或者用更通用的条件 `scope='user' AND status='active'` 一并保护。

## §7 Token 实际 walkthrough — T8 第 8 次改动后 manifest 账

T8 是一天 8 次改动的最后一次（加签到+邀请），也是 T9 compaction 前的"最高水位"，选它做账最能暴露压力。

**输入**：plan_state 累计 bump 到 version=8；累计 `session_state_item_events` ≈ 58 条；累计 session_todos rows = 26（10 active + 16 cancelled）；累计 decision 行 6（5 active + 1 superseded）；累计 summary 行 1（T4 产的 morning_revisions_v1，仍 active）。

**Budget template**: `budget_v1_32k`，总 context window 32k，预留 output 2k + safety 500 = 29.5k 可分配给 zones。

| Zone | cap（budget_v1_32k） | T8 实际 | 计算 | 是否 overflow |
| --- | --- | --- | --- | --- |
| system_static | 1500 | 1200 | 固定 prompt | ✅ |
| tool_schemas | 2000 | 1400 | PRD 工具集 8 个 × ~175 | ✅ |
| session_anchor | 500 | 320 | 目标 200 + 3 条 user-rule 120 | ✅ |
| plan_todo | **800（A11 硬）** | **620** | 10 active × ~58 = 580 + deps 40 | ✅（余 180） |
| summary | 600 | 380 | morning_revisions_v1 一条 | ✅ |
| retrieved_facts | 1500 | 720 | 6 decision × ~110 = 660 + 1 superseded 60 | ✅ |
| recent_tail | 3000（floor 1600） | 1400 | T6 + T7 + T8 前半 | ✅ |
| tool_previews | 500 | 0 | T8 未调工具 | ✅ |
| Reserved output | 2000 | 2000 | — | ✅ |
| Safety buffer | 500 | 500 | — | ✅ |
| **合计** | ~12900 | **8540** | — | ✅ 距离 32k 还很远 |

**关键观察**：

1. plan_todo 620 / cap 800：A11 硬上限没触发 title-only fallback（T5 触过一次，T7 大砍后回到安全区，T8 加 4 条仍有 180 余量）。如果 T8 再加 3 条 todo，就会触顶。
2. retrieved_facts 720：6 条 decision 各 ~110 tokens（key+rationale+status），老板砍 MVP 这条 priority='critical' 置顶。22 条 cancelled todo 的 description 完全不进 prompt（~5.5k tokens 留 DB）——这是 A11 + A6 组合省的大头。
3. 如果按 §6 组合 3 的建议把 user-scope engineering_rule 纳入 retrieved_facts（而非只塞 anchor），retrieved 会涨 ~150 tokens 到 870，仍在 cap 内。
4. **audit 回放代价**：plan_state 累计 8 次 bump + 预期 T11/T12 共 1 次 = v9，`session_state_item_events` 查询 `WHERE item_id='plan_state_main' ORDER BY id` 返回 9 行，按 L1116 previous_version/next_version 列直接串链无需 JSON 过滤。单次查询 < 5ms（idx_state_events_item_created 索引命中），回放 payload 反序列化 9 次 ~10ms，总 < 20ms。T10 的 "回到上午那版" 候选列表 (v1-v5) 直接拿前 5 行即可，无 LLM。
5. **朴素方案对比**：若无 A11 / A6，T8 prompt 会把 8 版 plan（~3.2k）+ 42 条 todo 全量（~6.5k）+ 8 轮原始对话（~5k）= 14.7k 全塞进去，本设计只用 3.44k（plan_todo+decision+summary+anchor+tail），**节省 ~77%**。更重要：决策理由从 prose 噪声里被结构化抽出，模型不会"复活折扣"。
6. **朴素方案会不会溢 32k 窗口？** 14.7k + 2600（system+tools）+ 2500（reserved+safety）= 19.8k，不会直接溢，但 recent_tail zone 会抢 12k 挤掉 retrieved_facts，结果就是"模型能看到原始对话但看不到 structured decision"——正好是 F1 失败模式的温床。

**结论**：T8 manifest 所有 zone 都在 cap 内，plan_todo 距顶 180 tokens，retrieved_facts 距 cap 780 tokens，dropped_count=0，`tokenizer_id='o200k_base'`、`budget_template_id='budget_v1_32k'`、`reason='plan_revision'`。**不会 overflow**；一天 8 次反转 + 9 版 plan bump 的全部结构化事实被固定在 `plan_todo+retrieved_facts+summary = 1720 tokens`，占总 prompt 20%，健康。

## §8 最终判断

- **本场景下 v0.2 是否可实现**: ✅（所有关键机制 G2/G6/G12/G14 都已契约化，schema 已有 backlog_pool_id / previous_version / next_version 列）
- **顺畅度**: ⚠️ 有明显摩擦（F1 inline compaction 时序冲突 / F2 场景原文与 G2 版本约束错位 / F3 cancel 的 mutation 归属歧义 / F5 cancel→backlog promote 策略缺失；3 条可落为新 gap G20–G22）
- **一句话总结**: v0.2 的"只动 summary 不动结构化事实 + scope='user' backlog pool + next-action 分级 state machine"三件套刚好扛住 8 次反转 + 歧义回溯 + 跨日续接，但 cancel↔backlog 的 transition 契约、compaction 与 run 的并发顺序、mutation 枚举对 cancel 的覆盖还有三条明确缺口需要补平。
