# S05 — PM 小王一天内改 8 次「会员积分商城」需求

> Status: Draft v0.1
> DAUT: **A6, A11, A15**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：小王，某电商创业公司 PM，3 年经验，擅长把老板一句话需求
拍成 PRD，但也被老板一天改 5 次方向锤得"哪版是哪版"自己都记不住。
周四从早到晚一天内把「会员积分商城」这个 feature 的形态改了 8 次，不
是她任性，是市场、合规、老板轮流轰炸：上午想冲 GMV，中午合规打回
抽奖，下午老板嫌重、砍 MVP，晚上自己睡一觉又想加签到。

**业务背景**：公司 APP 要在 Q3 上会员体系。小王用 astra web agent 边
讨论边让 agent 维护 plan + todo + PRD 骨架。**她不是在"写代码"**，她
是在"拍需求 + 看 agent 反馈 + 让 agent 产出 PRD 草稿 / wireframe
outline / 任务拆解"。关键是：每次改动都要让 agent 把之前已经被否决
的方向"记住是怎么被否决的"（不然下次某个环境一变又被捡回来）。

**一句话目标**：在一天之内把反复漂移的产品需求收敛到一个可交付的
MVP + follow-up backlog，每一次否决都留下可追溯的 decision，最终
session 关掉后第二天再开还能完整恢复"今天吵过的架"。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 12 回合 |
| 会话持续时间 | 10.5 小时（单日 09:00–19:30） |
| 需求反转次数 | 8 次（T1–T8） |
| 触发 compaction 次数 | 2 次（T4 inline + T9 dedicated） |
| session_todos 累计写入 | 约 42 条，最终 active ≤ 6 |
| decision 累计（不可删） | 7 条（至少 5 条是"被否决 + 为什么"） |
| plan_state version bump | 9 次（每次改动 +1，compaction 再 +1） |

## 2. 被测 DAUT

S05 的核心矛盾是「需求反复 × 短时间 × 高认知负荷」，下列 3 条假设被
同时压测。

| 编号 | 压测方式 |
| --- | --- |
| **A6** | T4 之后触发第一次 compaction，必须保留 L1a `decision` 行：折扣被砍的理由（成本 + UI 互斥）、抽奖被砍的理由（合规）——这两条 `category='decision'` 必须以 `status='active'` 留下；对应的 `session_todos` 只能被 `status='cancelled'`，**不允许** 物理删除。T9 第二次 compaction 再加入排行榜（stakeholder 觉得激化对立）、拼团/分享（老板砍 MVP）两条 decision，累计 decision 只增不减。 |
| **A11** | 每一回合的 `plan_todo` zone 只渲染**当前 active/in_progress** 的 `session_todos`，token 预算硬上限 800。即便 8 版需求叠加（42 条累计 todo），T8 之后 prompt 里看到的仍然是那一版当前有效的活跃清单，而不是 8 版拼接的 hydra 文档。`session_todo_deps` 在每次改动时重建依赖图，被取消的 todo 连同它的出边/入边一起迁到 `status='cancelled'`（依赖图只保留活跃节点）。 |
| **A15** | T10 小王说「继续」——`NextActionExtractor` 按优先级：structured event → rule → small model，直接命中最新 `plan_state`（version=9，MVP + 签到 + 邀请），把它作为续接对象，**不做 LLM 抽取**。T10 后半段小王说「不对不对，回到上午那版」——"上午那版"歧义（T1 纯积分兑换？T2 加折扣？T3 加抽奖？），低置信度不自动回滚，写入 `suggested_next_action` 并发起 `ask_user` 澄清。 |

## 3. 会话时间线

12 个回合覆盖：8 次需求反转（T1–T8）+ T9 dedicated compaction + T10
"继续"歧义测试 + T11 agent 真正开始执行 MVP + T12 交付小结。每回合
字段：**用户原话**（模拟真实 PM 语气，可能抱怨）、**state_items 变化**
（`session_todos` 状态迁移 / `session_todo_deps` 重建 / `decision` 追
加 / `plan_state` bump）、**manifest zone**（token 分配，重点 plan_todo
/ decision 保留区）、**LLM 看不到**（累计但不进 prompt 的内容）。

### T1 · 09:00 开局「积分兑换页面」

**用户原话**：「我们要给 APP 加个会员积分商城，第一步就做积分兑换
页面——用户用积分换实物/优惠券。你帮我拟个 plan 和初版 todo，别太
重，我要先跟老板过一下方向。」

**state_items 变化**：新增 `anchor`（目标：会员积分商城，阶段：需求
收敛）；`plan_state` version=1，phase='draft_v1_积分兑换'；
`session_todos` 批量 insert 6 条 P0 todo（兑换列表页 / 商品详情 / 兑
换流程 / 积分余额接口 / 下单幂等 / PRD 文档骨架），全部 `status=
'active'`，`source='agent'`；`session_todo_deps` 建 3 条依赖（兑换流程
← 详情 ← 列表、下单幂等 ← 兑换流程）；新增 1 条 `decision`
（key='product.goal.v1'，"MVP 先做积分兑换，不碰其他消耗路径"）。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1400（PM 场景
剪掉代码相关工具）/ `session_anchor` 300 / `plan_todo` 480（6 条
active todo + 3 条 deps）/ `recent_tail` 0 / `summary` 0 /
`retrieved_facts` 0。总 ≈ 3380，`reason='initial_turn'`。

**LLM 看不到**：无（起始点，没有历史）。

### T2 · 10:30「加多级会员折扣」

**用户原话**：「老板刚过完——他想加个多级会员折扣进去，银/金/钻三
档，用户等级越高兑换折扣越狠，说是要冲 GMV。你把这个加进 plan。」

**state_items 变化**：`plan_state` version=2，phase='draft_v2_兑换+折
扣'；`session_todos` 新增 4 条（会员等级模型 / 折扣规则配置 / 兑换
价展示层 / 折扣风控校验），全部 `active`；`session_todo_deps` 新增
4 条依赖（折扣规则 ← 等级模型、兑换价展示 ← 折扣规则、兑换流程 ←
折扣风控）；新增 1 条 `decision`（key='product.tier_discount.added'，
"老板要求加多级会员折扣冲 GMV，待验证成本"，`status='tentative'`）。

**manifest zone**：`plan_todo` 680（10 条 active） / `recent_tail` 1200
（T1 回合 + 本轮前半段）/ `session_anchor` 300。总 ≈ 4300，
`dropped_count=0`。

**LLM 看不到**：老板沟通的原始 IM 聊天记录（小王没贴全，只贴了结
论）。

### T3 · 11:45「折扣砍掉，加抽奖」

**用户原话**：「等下算了——财务那边说多级折扣要重做会员积分账本
成本太高，而且兑换价 UI 位本来就紧张。砍掉折扣。但是运营这边想
加个积分抽奖，说是日活拉新用。」

**state_items 变化**：`plan_state` version=3，phase='draft_v3_兑换+抽
奖'；**折扣相关 4 条 `session_todos` 批量 UPDATE 到 `status=
'cancelled'`**（不物理删）；`session_todo_deps` 里指向/来自它们的 4
条依赖标 `valid=false`（图投影只取 active 节点）；新增 3 条抽奖 todo
（抽奖转盘页 / 奖池配置 / 概率风控），`active`；**新增 1 条
`decision`**（key='product.tier_discount.rejected'，`status='active'`，
payload 含 rationale："cost=会员账本重做 + UI=与兑换价位互斥"），
前一轮那条 tentative 的 discount decision 被 supersede 为
`status='superseded'`。

**manifest zone**：`plan_todo` 720（9 条 active todo：6 原始 + 3 抽奖，
折扣 4 条已被 A11 滤掉不进 prompt） / `decision` 区 + 160（新 decision
进 `retrieved_facts` 前排） / `recent_tail` 1400 / `session_anchor` 300。
总 ≈ 4580。`reason='plan_revision'`，`dropped_count=4`（cancelled
todo）。

**LLM 看不到**：4 条 cancelled 折扣 todo 的完整 description（累计约
1100 tokens），它们还在 DB，UI 「已取消」折叠区能展开。

### T4 · 13:00「抽奖合规不行，改拼团」（触发首次 compaction）

**用户原话**：「中午合规找我——抽奖归文化监管要申请许可证，小公
司根本办不下来，而且 App Store 审核会卡。抽奖也砍。改成拼团吧：
用户拉好友成团兑换，成团价低、不成团退积分，合规风险小。」

**state_items 变化**：`plan_state` version=4，phase='draft_v4_兑换+拼
团'；3 条抽奖 `session_todos` → `status='cancelled'`；新增 5 条拼团
todo（开团发起页 / 团详情 / 凑团分享组件 / 成团回调结算 / 退积分兜
底），`active`；`session_todo_deps` 重建（凑团分享 ← 开团、成团结算
← 开团 + 团详情、退积分兜底 ← 成团结算）；**新增 1 条 `decision`**
（key='product.lottery.rejected'，payload rationale："compliance=文化
监管许可 + appstore_review_risk"，`status='active'`）；抽奖那条前序
decision 若有（T3 未写）不追加。**CompactionPolicy 观察到** `recent_
tail` 已累计 ~4 个回合 / 9600 tokens + plan 已经 version=4 且每次都
大改，触发**首次 compaction**：把 T1–T3 的 recent_tail 压成 1 条
`summary`（L1b，key='summary:morning_revisions_v1'，token_estimate
≈ 380），明确列出「09:00 起方向：兑换 → 兑换+折扣 → 兑换+抽奖 →
兑换+拼团」以及每次否决原因；`session_todos` 和 `decision` 都**不**
被 compaction 改写。

**manifest zone**：`plan_todo` 740（6 原始 + 5 拼团 = 11 active，刚好
撞近 800 上限） / `summary` 380（新 L1b） / `retrieved_facts` 360
（折扣 decision + 抽奖 decision 两条结论原文，供 agent 下次别提它
们）/ `recent_tail` 700（压缩后只剩 T4 本轮）/ `session_anchor` 300。
总 ≈ 4880，`compaction_version='v1.0'`，`reason='post_compaction_turn'`，
`dropped_count=3`（cancelled 抽奖 todo）。

**LLM 看不到**：T1–T3 原始对话（~7000 tokens），折扣和抽奖 todo 的
full description（~1800 tokens），合规同事发给小王的钉钉截图附件
（已存 `session_artifacts` 但不进 prompt）。

### T5 · 14:30「拼团 + 分享攒积分 + 排行榜」

**用户原话**：「拼团这个方向老板看了挺认可，他又加码了——让用
户分享链接攒积分，再搞一个分享积分排行榜，top 100 发实物奖品，
他说要『做成一个小型社交事件』。你把这些加进去。」

**state_items 变化**：`plan_state` version=5，phase='draft_v5_兑换+拼
团+分享+排行榜'；新增 6 条 todo（分享链接生成服务 / 分享积分结算
/ 分享防刷风控 / 排行榜页 / 排行榜每日榜结算 job / 排行榜奖品发放
流程），active；`session_todo_deps` 新增 6 条（分享积分 ← 分享链接、
排行榜页 ← 分享积分结算、奖品发放 ← 每日榜结算 job 等）；新增 1
条 `decision`（key='product.social_expansion.added'，`status=
'tentative'`，"老板指令加分享+排行榜，待成本评估"）。

**manifest zone**：`plan_todo` 790（17 active，压到 800 上限，agent
自动把 3 条优先级最低的 P2 细节项折叠成 title-only）/ `summary`
380 / `retrieved_facts` 360 / `recent_tail` 1300 / `session_anchor` 300。
总 ≈ 4730。`reason='plan_revision'`，`dropped_count=0`（还没到强 drop
阈值，但 plan_todo 的渲染器已启动 title-only fallback）。

**LLM 看不到**：无重大遮蔽；但 plan_todo 已经开始吃预算压力，是
A11 硬上限在起作用的明显信号。

### T6 · 15:45「排行榜砍，保留好友助力」

**用户原话**：「排行榜这个点我们内部讨论过，觉得刺激用户之间竞争
容易激化对立情绪，而且榜上名次打架我们客服接不住。砍掉排行榜。
但是好友助力这个形式留下——让好友点一下帮你攒积分就行，不做
排名。」

**state_items 变化**：`plan_state` version=6，phase='draft_v6_兑换+拼
团+分享+好友助力'；3 条排行榜相关 `session_todos` → `status=
'cancelled'`；新增 2 条好友助力 todo（助力按钮/H5 落地 / 助力防刷
+ 上限）；`session_todo_deps` 重建；**新增 1 条 `decision`**
（key='product.leaderboard.rejected'，payload："激化用户对立 + 客服
承接不了榜位纠纷"，`status='active'`）。累计 decision：折扣 rejected
/ 抽奖 rejected / 社交加码 tentative / 排行榜 rejected = 4 条。

**manifest zone**：`plan_todo` 770（17 - 3 + 2 = 16 active） / `summary`
380 / `retrieved_facts` 540（4 条 decision 进入 top-K） / `recent_tail`
1300 / `session_anchor` 300。总 ≈ 4860。

**LLM 看不到**：排行榜 3 条 cancelled todo 原文；客服团队同事在内
部群的吐槽原文（已落 `session_artifacts` 但不进 prompt）。

### T7 · 17:00「老板砍 MVP，只留积分 + 兑换」

**用户原话**：「完了完了——老板刚拉我进会议室，他说公司下个月融
资路演，Q3 不能背这么重的包，让我**只做积分 + 兑换**，其他全砍。
拼团、分享、好友助力、排行榜全砍。你把 plan 砍到 MVP。」

**state_items 变化**：`plan_state` version=7，phase='mvp_v1_纯积分
兑换'；**批量**把拼团 5 条 + 分享 3 条 + 好友助力 2 条 = 10 条
`session_todos` → `status='cancelled'`；`session_todo_deps` 里对应 12
条边标 `valid=false`；**新增 1 条 `decision`**（key='product.mvp_cut_
by_boss'，payload："Q3 融资路演，老板指令砍到只保留积分+兑换；
拼团/分享/好友助力/排行榜全部推到 post-MVP backlog"，`status=
'active'`，priority='critical'）；前一轮那条 `social_expansion.added`
的 tentative decision 被 supersede 为 `status='superseded'`。active
todo 回到最初 6 条。

**manifest zone**：`plan_todo` 460（6 active，大幅收窄） / `summary`
380 / `retrieved_facts` 640（5 条 decision，老板砍 MVP 这条 priority=
critical 优先置顶）/ `recent_tail` 1400 / `session_anchor` 320。总 ≈
4400，`dropped_count=10`（批量 cancelled），`reason='plan_scope_cut'`。

**LLM 看不到**：10 条 cancelled todo 的 description（~2800 tokens）
留 DB；老板会议室口头沟通没有原文（小王只口述了结论）。

### T8 · 19:30「加签到和邀请拿积分」

**用户原话**：「回家路上我又想了一下——MVP 里积分只有『兑换』这
一个出口，但『怎么赚积分』这条路径太单薄了。我加两个轻量的入
口：每日签到拿积分 + 邀请新人注册拿积分。这两个都不涉及合规也
不涉及大开发量，老板应该认。」

**state_items 变化**：`plan_state` version=8，phase='mvp_v2_积分+兑
换+签到+邀请'；`session_todos` 新增 4 条（每日签到页 / 签到积分结
算 / 邀请链接生成 / 邀请注册回调结算），active；`session_todo_deps`
新增 4 条（签到结算 ← 签到页、邀请结算 ← 邀请链接等）；新增 1
条 `decision`（key='product.earn_path.added'，"签到+邀请作为积分获
取入口，合规低风险"，`status='active'`）。

**manifest zone**：`plan_todo` 620（10 active） / `summary` 380 /
`retrieved_facts` 720（6 条 decision，被老板砍的优先级置顶） /
`recent_tail` 1400 / `session_anchor` 320。总 ≈ 4740。
`reason='plan_revision'`。

**LLM 看不到**：小王回家路上的原始思路（她没贴微信草稿）；8 次反
转里累计 20 条已 cancelled 的 todo（累计约 5.5k tokens）。

### T9 · 20:00 第二次 compaction（dedicated）

**用户原话**：（无；CompactionPolicy 基于 `recent_tail` 累计阈值 +
「plan 在一天内 version bump ≥ 7」的规则自动触发，小王下班前看了
下进度条。）

**state_items 变化**：新写一条 `summary`（key='summary:day_one_
converged_v1'，token_estimate ≈ 520），叙述「一天 8 次反转，最终
落在积分+兑换+签到+邀请 MVP，4 大方向被否决（折扣/抽奖/排行榜/
社交加码）」；**旧 summary（T4 产出的 morning_revisions_v1）被标
`status='archived'`，payload 保留不删**；`plan_state` version=9（compaction
带来的结构化重排，不是需求本身改动）；6 条 active `decision` 全部
保持 `status='active'`（compaction **不可**改写）；cancelled `session_
todos` 累计 20 条全部保留；`session_state_item_events` 批量写 22 条
mutation（audit）。

**manifest zone**：`summary` 520（新）/ `plan_todo` 620（10 active，
未变）/ `retrieved_facts` 720 / `session_anchor` 320 / `recent_tail`
800（压缩后只保留 T7–T8 最近 tail）。总 ≈ 4220，`compaction_
version='v1.1'`，`reason='post_compaction_turn'`，`dropped_count=0`
（cancelled todo 本来就没在 prompt，归档也不改）。

**LLM 看不到**：T1–T6 的原始对话（~12k tokens），旧 summary
（被 archived 但保留），20 条 cancelled todo 的完整 description。

### T10 · 次日 09:00「继续」/「回到上午那版」（A15 歧义测试）

**用户原话**：「（上午打开 session）继续吧。……等等，不对不对，回
到上午那版。」

**state_items 变化**：小王说「继续」——`NextActionExtractor` 优先级
执行：step 1 **structured event** 命中 `plan_state` version=9（MVP+
签到+邀请），直接以此为续接对象，**不调 LLM 抽取**；生成 1 条
`suggested_next_action`（`source='structured_event'`，`confidence=
'high'`，引用 `plan_state` v9）。小王紧接着说「回到上午那版」——
rule extractor 识别出"上午那版"是回溯意图但**指代不明**（候选：T1
纯兑换 / T2 加折扣 / T3 加抽奖 / T5 加社交 / T7 老板砍 MVP），低
置信度：不自动 rollback `plan_state`，而是写入一条
`suggested_next_action`（`source='rule'`，`confidence='low'`，
`expires_at=+1h`，candidates=[v1,v2,v3,v5,v7]），**并发起
`ask_user`** 澄清："您说的『上午那版』是指以下哪一版？（1）09:00
纯积分兑换；（2）10:30 加折扣那版；……" agent **不动**
`session_todos`、**不动** `plan_state`。

**manifest zone**：`plan_todo` 620（当前 MVP v9 照旧） / `suggested_
next_action` 新 zone 220（2 条 suggestion 并列，含高置信接续 + 低
置信歧义） / `summary` 520 / `retrieved_facts` 720（把 6 条
decision + 旧 plan 的各版 snapshot title 都拉出来供小王对照） /
`recent_tail` 400（只有本轮） / `session_anchor` 320。总 ≈ 4800，
`reason='continue_with_ambiguity'`，`dropped_count=0`。

**LLM 看不到**：小王昨晚回家路上跟闺蜜的微信聊天（不在 session
里）；若小王之后真选了 v3（加抽奖），合规 decision 仍然以
`retrieved_facts` 形式进 prompt，防止 agent "忘了之前为什么否决"
再次把抽奖加回来。

### T11 · 09:30 小王确认后 agent 执行 MVP

**用户原话**：「算了不回滚了——就按昨晚那版（积分+兑换+签到+邀
请）推进。你先把 PRD 草稿出来，tech lead 要看。」

**state_items 变化**：T10 的 low-confidence suggestion `status=
'resolved_by_user_choice'`（`expires_at` 提前到 now）；高置信接续
`status='accepted'`；`plan_state` 保持 version=9；10 条 active
`session_todos` 中「PRD 文档骨架」那条 `status='in_progress'`，由
agent 开始实际渲染 PRD；agent 产出的 PRD 落 `session_artifacts`
（`artifact_ref='artifact:prd-integral-mall-mvp-v1'`，`byte_size≈48KB`，
preview_text 500 字符进 prompt）；新增 1 条 `tool_ref`（render_
prd），`token_estimate=180`。

**manifest zone**：`plan_todo` 640（10 active，其中 1 条 in_progress
高亮）/ `summary` 520 / `retrieved_facts` 720（6 条 decision 仍全量
保留，因为 PRD 草稿要在"遗留风险与否决方向"章节引用它们）/
`tool_previews` 180（PRD preview）/ `recent_tail` 700 / `session_
anchor` 320。总 ≈ 4520，`reason='execute_after_clarification'`。

**LLM 看不到**：PRD artifact 全文（48KB）；小王和 tech lead 的私聊
（不在 session）。

### T12 · 11:00 交付小结

**用户原话**：「PRD 给 tech lead 看过了，他说 OK。你帮我写一份今
天的 decision 清单，我贴到周报里，要能看出为什么砍、为什么加。」

**state_items 变化**：agent 发起一次**纯 structured 检索**：`SELECT
* FROM session_state_items WHERE session_id=? AND category=
'decision' AND status IN ('active','superseded') ORDER BY priority
DESC, created_at ASC`，一次拿到全部 7 条 decision + 2 条 superseded，
渲染为"今日决策日志"章节（含 decision_key / 加入或否决 / rationale
/ 触发人）；同时一次 `plan_state` 历史查询拿到 v1→v9 的 phase 迁
移轨迹。产出 1 条 `session_artifacts`（`artifact:decision-log-day1`），
并新增 1 条 `anchor` 子项（"day_one_delivered"）。

**manifest zone**：`retrieved_facts` 破例扩到 1800（7 条 decision +
9 版 plan phase 名单全量进入交付）/ `plan_todo` 260（仅剩交付确认
todo）/ `summary` 520 / `session_anchor` 400。总 ≈ 5000，
`reason='final_delivery_summary'`。

**LLM 看不到**：T1–T11 的完整 transcript（~40k tokens）。交付文档按
decision / plan_phase 这两条 L1a 轴合成，绝不回放原始对话。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 8 次反转，42 条 todo 累计，但 prompt 每轮 plan_todo ≤ 800 tokens | A11：render 只取 `session_todos.status IN ('active','in_progress')`；cancelled 保留 DB 不渲染 |
| 每次反转都要让 agent "记住" 被否决的方向 & 原因 | A6：`category='decision'` 的 active 行在 compaction 时不可改写；retrieved_facts 每轮都注入最新 N 条 decision |
| 短时间内 plan_state version bump 9 次，不能让 agent 跑错 phase | A11 + A6：`plan_state` 一个 item_key 单行存 latest，`session_state_item_events` 留版本 audit；agent 永远读当前行而非拼老版本 |
| T4 + T9 两次 compaction 不能把"为什么砍折扣/抽奖/排行榜/社交加码"叙述化后丢掉结构 | A6：compaction 只动 `summary` 和 `tool_ref`/`active_file`，**不动** decision 和 active todo |
| T10 "继续" 和 "回到上午那版" 两句话混在一起，语义歧义 | A15：structured event 优先（高置信直接接续）；rule 识别回溯意图但候选多 → `suggested_next_action` + `ask_user`，**禁止自动回滚** |
| 需求 cancel 后 `session_todo_deps` 图仍然合法（不能悬挂） | 重建：cancelled todo 的出边/入边标 `valid=false`；context builder 渲染图时只遍历 active 节点 |
| 一天内 7 条 decision 中 5 条是"否决式"，回退历史就要它们不被挤出预算 | retrieved_facts 按 `priority` + `decision.status='active'` 排序，老板砍 MVP 这条 priority=critical 永远进第一梯队 |

## 5. 预期 state projection 演化

下面是关键节点快照（按 category 分组 row 数，括号内为 `status=
'active'`；`session_todos` 单独列因为它不在 `session_state_items`）。

**T1（09:00 开局后）**：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| anchor | 1 | 1 | 会员积分商城目标 |
| plan_state | 1 | 1 | version=1，draft_v1_积分兑换 |
| decision | 1 | 1 | product.goal.v1 |
| session_todos | 6 | 6 | 初始 6 条 P0 |
| session_todo_deps | 3 | 3 | 兑换流程链路 |
| summary | 0 | 0 | — |

**T4（13:00 首次 compaction 后）**：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| anchor | 1 | 1 | — |
| plan_state | 1 | 1 | version=4，draft_v4_兑换+拼团 |
| decision | 3 | 2 active + 1 superseded | 折扣 rejected / 抽奖 rejected / 折扣 tentative 被 supersede |
| session_todos | 18 | 11 | 6 原始 + 4 折扣（cancelled） + 3 抽奖（cancelled） + 5 拼团 |
| session_todo_deps | 11 有效边 | 10 active | cancelled 节点的边 valid=false |
| summary | 1 | 1 | morning_revisions_v1（L1b） |

**T7（17:00 老板砍 MVP 后）**：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| anchor | 1 | 1 | — |
| plan_state | 1 | 1 | version=7，mvp_v1_纯积分兑换 |
| decision | 5 | 4 active + 1 superseded | +排行榜 rejected / +老板砍 MVP（critical）/ 社交加码 tentative → superseded |
| session_todos | 26 | 6 | 6 原始 active；拼团 5 + 分享 3 + 好友助力 2 + 抽奖 3 + 折扣 4 = 17 cancelled +  排行榜 3 cancelled |
| session_todo_deps | 有效 3 | 3 active | 回到最初依赖图 |
| summary | 1 | 1 | morning_revisions_v1 仍 active |

**T8（19:30 加签到+邀请后）**：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| plan_state | 1 | 1 | version=8，mvp_v2_积分+兑换+签到+邀请 |
| decision | 6 | 5 active + 1 superseded | + earn_path.added |
| session_todos | 30 | 10 | +4 签到/邀请 active |
| session_todo_deps | 有效 7 | 7 active | +4 签到/邀请依赖 |

**T12（交付）**：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| anchor | 2 | 1 | 交付确认子项为 status=final |
| plan_state | 1 | 1 | version=9（T9 compaction 后 bump） |
| decision | 7 | 5 active + 2 superseded | 交付日志引用全部 |
| session_todos | 30 | 10 | 结构未动，PRD 骨架 in_progress → done |
| summary | 2 | 1 | v1 archived，v2 active |

**关键不变量**：physical delete 计数 **= 0**；任何 cancelled/
superseded 行都能通过 `session_state_item_events` 或 DB 行本身完整
回溯；`decision` 行数严格单调递增。

## 6. Token 节省估算

**朴素方案**（8 版 plan + 8 版 todo + 全量对话全塞第 8 轮 prompt）：

- 每版 plan phase + rationale 描述 ≈ 400 tokens × 8 = 3200
- 每版 todo 清单（含 cancelled）累计描述 ≈ 1500 tokens × 8 = 12000
  （因为朴素方案不懂"只渲染 active"，8 版 todo 拼接会把 42 条全部
  带上，哪怕已 cancelled 也一字不删）
- 8 轮用户对话 + agent 答复叠加 ≈ 5000 tokens
- **朴素总计**：3200 + 12000 + 5000 ≈ **20.2k tokens** 塞进第 8 轮
  prompt

**本设计方案**（T8 实际进 prompt 的 token，按 zone 重点列）：

| zone | T8 | 说明 |
| --- | --- | --- |
| plan_todo | 620 | 10 条 active todo；cancelled 22 条完全不渲染 |
| decision（retrieved_facts） | 720 | 6 条 decision，含老板砍 MVP（critical 置顶）+ 4 条否决理由 |
| summary | 380 | 上午 4 次反转的 L1b 叙述 |
| recent_tail | 1400 | T6–T8 原文（约 3 轮） |
| session_anchor | 320 | 目标 + 当前 phase |
| system_static + tool_schemas | 2600 | 常驻 |
| **有效"会话状态"合计** | **≈ 3440** | plan_todo + decision + summary + anchor + recent_tail |

**对比**：

| 指标 | 朴素方案 | 本设计 | 节省 |
| --- | --- | --- | --- |
| 会话状态部分 | 20.2k | 3.44k | **83%** |
| 失去"否决理由"风险 | 无（全塞，但模型在噪声中未必看到） | 否决理由以 decision 结构化高亮 | 质量提升 |
| cancelled todo 是否污染 prompt | 是（22 条），模型可能"复活" 已砍项 | 否（硬过滤） | 准确性提升 |

**计算式**：节省比例 = (20200 − 3440) / 20200 ≈ **82.97%**。更重要
的不是数值本身，而是**把"被否决的方向 + 原因"从噪声里拎出来变结
构化一等事实**：朴素方案把 8 版 todo 全塞，模型在 20k 噪声里可能
把"折扣"又加回来；本设计方案用 720 tokens 的 decision 区永远告诉
模型"折扣已 rejected，理由是成本 + UI 互斥"。

## 7. 潜在失败模式

**F1 · compaction 把 cancelled 方向的理由叙述化后丢结构**：如果 T4
/ T9 的 compaction 为了压 token 把"折扣 rejected 因为成本 + UI 互
斥"揉进 `summary` 自然语言里，然后把 `decision` 行也压掉或标
archived，下次用户再提折扣 agent 只能模糊地说"之前砍了"，无法说
清理由。**可观测信号**：T5 之后 `SELECT COUNT(*) FROM session_
state_items WHERE category='decision' AND status='active'` 返回 < 4；
agent 在 T11 PRD 草稿里对"为什么不做折扣"语焉不详。

**F2 · plan_todo zone 渲染把 cancelled todo 也带进来**：如果
ContextAssembler 懒得按 status 过滤，直接 `SELECT * FROM session_
todos WHERE session_id=?`，T8 plan_todo zone 会从 ~620 tokens 膨
胀到 ~3000+，A11 的 800 硬上限形同虚设，agent 会把 cancelled 的
拼团 todo 也当任务执行。**可观测信号**：T8 `context_manifests.
total_estimated_tokens` 跳涨；`context_manifest_items` 里 zone=
'plan_todo' 的行数 > 15；agent 输出提到"下一步做拼团开团页"。

**F3 · T10「继续」被 LLM 抽取而非 structured event 命中**：如果
`NextActionExtractor` 没有 structured 短路，直接把"继续"两个字丢
给 small model 去抽，会浪费调用成本，且在小王立刻改口说"回到上
午那版"时产生竞态——两个 suggested_next_action 按时间序写入，
但 agent 可能采用第一个（已过期）。**可观测信号**：`context_
manifests.reason` 在 T10 不是 `continue_with_ambiguity` 而是
`llm_next_action_extraction`；`suggested_next_action` 的
`source='small_model'`。

**F4 · T10「回到上午那版」被自动回滚 plan_state**：如果 rule
extractor 把"上午那版"简单映射到"morning_revisions_v1 summary"
然后把 `plan_state` 直接 bump 回 version=1，会让 MVP 成果瞬间消
失、T11 agent 渲染 PRD 时写错版本。**可观测信号**：T10 结束
`plan_state.version` 从 9 变成 1；`session_state_item_events` 没有
对应的 `ask_user` event；小王下一句是"你怎么把签到砍了？"

**F5 · session_todo_deps 不重建，依赖图悬挂**：如果 cancel 一条
todo 时只改 `session_todos.status` 但不动依赖边，当 agent 按依赖
图推进时会在 cancelled 节点上悬挂/死循环。**可观测信号**：图遍历
日志出现 "next candidate: todo:lottery-prob-risk (status=cancelled)
— skipped"；agent 卡在 "等待依赖完成"。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Plans and Todos — **现有设计缺口**：`session_todos.session_id`
   是单 session 的，但真实产品场景里，MVP 砍掉的 post-MVP backlog
   （S05 的拼团/分享/排行榜）下个迭代要重新激活，往往是**新开一个
   session** 讨论。当前 schema 没有 `parent_session_id` 或者
   `backlog_pool_id` 之类的字段，意味着第二天新开 session 想捡起
   backlog 要么手动复制 todo（丢 provenance），要么跨 session 查询
   走不了 `idx_todos_session_position` 索引（退化成全库扫）。
2. §Proposed Schema §2 `session_state_items` — **plan_state 版本
   化未显式**：文档列了 `plan_state` 作为 category，但没说 version
   是靠 `session_state_item_events.id` 推断，还是 `payload_json.
   version` 冗余，还是 `provenance_event_id` 串链。S05 一天 bump
   9 次版本，如果没有明确的版本字段和版本回放 API，T10 的"回到上
   午那版"根本没法在候选里列出 v1/v2/v3/v5/v7 这 5 个快照。
3. §Next-Action Extraction Cost — **歧义处理流程未闭环**：文档说
   低置信度进 `suggested_next_action` 而非自动执行，但没说"同一轮
   用户一句话里同时命中高置信（"继续"）+ 低置信（"回到上午那版"）
   时怎么组合"。S05 的 T10 要求：高置信 suggestion 照常产出并
   accept，低置信 suggestion **另起** `ask_user`，两者并存而非互相
   覆盖。建议补一段 state machine。

**三条具体建议**：

- **跨 session plan/todo 池化**：新增可选字段 `session_todos.
  backlog_pool_id VARCHAR(128) NULL`，同 `user_id` 下可复用；
  cancel 时允许 `status='backlog'`（第三态），下次新 session 打开
  时按 pool 直接挂 plan，`provenance_ref` 指向原 session。
- **plan_state 显式版本化**：在 `session_state_items` 里对 category=
  `plan_state` 的行规定 `payload_json.version INT NOT NULL` +
  `payload_json.previous_version INT NULL`，并在文档里给出"回溯到
  version=k"的查询模式（走 `session_state_item_events` audit 流回
  放 payload）。当前只靠 item_events.id 的时间序隐式版本，对 T10
  这种歧义回溯不够用。
- **「继续」语义 state machine 图示**：在 §Next-Action Extraction
  Cost 末尾补一张小状态图（structured hit → accept /  rule hit
  ambiguous → ask_user / small_model low confidence →
  suggested_next_action），并规定一轮 turn 最多产出 N 条
  suggestion、不同 source 可并存但不可互相覆盖。S05 的 F3/F4 两
  个失败模式本质都是这条流程没闭环。
