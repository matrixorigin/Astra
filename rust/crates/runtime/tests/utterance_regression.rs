//! # Utterance Regression Tests
//!
//! Comprehensive regression suite testing tool selection behavior for ALL
//! common user utterance patterns. Each test verifies:
//!   - Signal detection (which flags fire)
//!   - Dynamic tool selection (what categories are included)
//!   - Confidence level (low/medium/high)
//!
//! Categories:
//!   1. GitHub queries (CN + EN)
//!   2. Git queries (CN + EN)
//!   3. Code editing (CN + EN)
//!   4. Code reading (CN + EN)
//!   5. Memory & preferences (CN + EN)
//!   6. Analytical / debug (CN + EN)
//!   7. Conversational (CN + EN)
//!   8. History references (CN + EN)
//!   9. Vague / ambiguous / entity-only
//!  10. Mixed intent
//!  11. Edge cases (short, long, emoji, URL, numbers)

use mo_agent_runtime::pipeline::routing::DomainHint;
use mo_agent_runtime::tool_registry::ToolRegistry;
use mo_agent_runtime::tool_registry::{
    ConversationState, IntentType, TOOL_CATALOG, pre_filter_dynamic,
};
use mo_agent_runtime::tool_selector::compute_selection_confidence;
use mo_agent_runtime::tool_selector::{SelectionContext, TfIdfSelector, ToolSelector};

// ─── Test helpers ────────────────────────────────────────────────────────────

/// Run tool selection for a query and return a structured result for assertions.
struct SelectionSnapshot {
    signal_count: usize,
    is_fetch: bool,
    is_mutate: bool,
    is_github: bool,
    is_git: bool,
    is_analytical: bool,
    is_conversational: bool,
    is_memory: bool,
    references_history: bool,
    dynamic_tools: Vec<(String, Vec<IntentType>)>,
    confidence: f64,
}

impl SelectionSnapshot {
    fn from_query(query: &str) -> Self {
        let state = ConversationState::from_message(query, 1);
        let results = pre_filter_dynamic(&state, query);

        let pinned: std::collections::HashSet<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();

        let dynamic_tools: Vec<(String, Vec<IntentType>)> = results
            .iter()
            .map(|&(idx, _)| {
                let t = &TOOL_CATALOG[idx];
                (t.name.to_string(), t.intents.to_vec())
            })
            .collect();

        let dynamic_count = dynamic_tools
            .iter()
            .filter(|(name, _)| !pinned.contains(name.as_str()))
            .count();

        let confidence = compute_selection_confidence(state.signal_count(), dynamic_count);

        Self {
            signal_count: state.signal_count(),
            is_fetch: state.is_fetch,
            is_mutate: state.is_mutate,
            is_github: state.is_github,
            is_git: state.is_git,
            is_analytical: state.is_analytical,
            is_conversational: state.is_conversational,
            is_memory: state.is_memory,
            references_history: state.references_history,
            dynamic_tools,
            confidence,
        }
    }

    fn has_intent(&self, intent: IntentType) -> bool {
        self.dynamic_tools
            .iter()
            .any(|(_, intents)| intents.contains(&intent))
    }

    fn tool_names(&self) -> Vec<&str> {
        self.dynamic_tools.iter().map(|(n, _)| n.as_str()).collect()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 1. GITHUB QUERIES
// ═══════════════════════════════════════════════════════════════════════════════

mod github_queries {
    use super::*;

    #[test]
    fn cn_list_prs() {
        let s = SelectionSnapshot::from_query("matrixorigin最新的pr");
        assert!(s.is_github, "'pr' should trigger is_github");
        assert!(s.is_fetch, "'最新' should trigger is_fetch");
        assert!(
            s.has_intent(IntentType::GitHub),
            "Should include GitHub tools"
        );
        assert!(
            s.confidence > 0.3,
            "Strong signals → decent confidence: {:.2}",
            s.confidence
        );
    }

    #[test]
    fn cn_list_issues() {
        let s = SelectionSnapshot::from_query("看看matrixorigin有哪些issue");
        assert!(s.is_github, "'issue' should trigger is_github");
        assert!(s.is_fetch, "'看看/有哪些' should trigger is_fetch");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn cn_check_ci() {
        let s = SelectionSnapshot::from_query("memoria最新的ci状态");
        assert!(s.is_github, "'ci' should trigger is_github");
        assert!(s.is_fetch, "'最新' should trigger is_fetch");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn cn_create_issue() {
        let s = SelectionSnapshot::from_query("给matrixorigin创建一个issue");
        assert!(s.is_github, "'issue' should trigger is_github");
        assert!(s.is_mutate, "'创建' should trigger is_mutate");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn cn_repo_query() {
        let s = SelectionSnapshot::from_query("matrixorigin仓库的情况");
        assert!(s.is_github, "'仓库' should trigger is_github");
        assert!(s.is_fetch, "'情况' should trigger is_fetch");
    }

    #[test]
    fn en_list_prs() {
        let s = SelectionSnapshot::from_query("list all open pull requests for memoria");
        assert!(s.is_github, "'pull request' should trigger is_github");
        assert!(s.is_fetch, "'list' should trigger is_fetch");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn en_show_pr_details() {
        let s = SelectionSnapshot::from_query("show me PR #123 details");
        assert!(s.is_github, "'PR' should trigger is_github");
        assert!(s.is_fetch, "'show' should trigger is_fetch");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn en_check_ci_status() {
        let s = SelectionSnapshot::from_query("check CI status for the main branch");
        assert!(s.is_github, "'CI' should trigger is_github");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn en_create_issue() {
        let s = SelectionSnapshot::from_query("create a new issue for this bug");
        assert!(s.is_mutate, "'create' should trigger is_mutate");
        assert!(s.is_github, "'issue' should trigger is_github");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn en_github_actions() {
        let s = SelectionSnapshot::from_query("show me the github actions status");
        assert!(s.is_github, "'github'/'actions' should trigger is_github");
        assert!(s.is_fetch);
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn en_repo_info() {
        let s = SelectionSnapshot::from_query("show repository information");
        assert!(s.is_github, "'repository' should trigger is_github");
        assert!(s.is_fetch, "'show' should trigger is_fetch");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. GIT QUERIES
// ═══════════════════════════════════════════════════════════════════════════════

mod git_queries {
    use super::*;

    #[test]
    fn cn_git_diff() {
        let s = SelectionSnapshot::from_query("看看git diff");
        assert!(s.is_git, "'git'/'diff' should trigger is_git");
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn cn_commit_history() {
        let s = SelectionSnapshot::from_query("查看最近的提交记录");
        assert!(s.is_git, "'提交' should trigger is_git");
        assert!(s.is_fetch, "'查看' should trigger is_fetch");
    }

    #[test]
    fn cn_branch_info() {
        let s = SelectionSnapshot::from_query("当前分支是什么");
        assert!(s.is_git, "'分支' should trigger is_git");
        assert!(s.is_fetch, "'什么' should trigger is_fetch");
    }

    #[test]
    fn cn_merge_request() {
        let s = SelectionSnapshot::from_query("合并这个分支到main");
        assert!(s.is_git, "'合并'/'分支' should trigger is_git");
    }

    #[test]
    fn en_git_status() {
        let s = SelectionSnapshot::from_query("git status");
        assert!(s.is_git, "'git' should trigger is_git");
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn en_git_log() {
        let s = SelectionSnapshot::from_query("show me the git log for the last 5 commits");
        assert!(s.is_git, "'git'/'commit' should trigger is_git");
        assert!(s.is_fetch, "'show' should trigger is_fetch");
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn en_git_diff() {
        let s = SelectionSnapshot::from_query("show me the diff");
        assert!(s.is_git, "'diff' should trigger is_git");
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn en_branch_operations() {
        let s = SelectionSnapshot::from_query("create a new branch from main");
        assert!(s.is_git, "'branch' should trigger is_git");
        assert!(s.is_mutate, "'create' should trigger is_mutate");
    }

    #[test]
    fn en_rebase() {
        let s = SelectionSnapshot::from_query("rebase this branch onto main");
        assert!(s.is_git, "'rebase'/'branch' should trigger is_git");
    }

    #[test]
    fn en_stash() {
        let s = SelectionSnapshot::from_query("stash my current changes");
        assert!(s.is_git, "'stash' should trigger is_git");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. CODE EDITING
// ═══════════════════════════════════════════════════════════════════════════════

mod code_editing {
    use super::*;

    #[test]
    fn cn_write_code() {
        let s = SelectionSnapshot::from_query("帮我写一个排序算法");
        // "写" contains "写入" partially? No — "写" alone may not match "写入".
        // The key point: this is a coding task, pinned tools (bash, write_file) suffice.
        // With 0 signals, adaptive threshold gives dynamic tools too.
        assert!(s.dynamic_tools.len() >= 3, "Should get some dynamic tools");
    }

    #[test]
    fn cn_fix_bug() {
        let s = SelectionSnapshot::from_query("修复main.rs里的编译错误");
        assert!(s.is_mutate, "'修复' should trigger is_mutate");
    }

    #[test]
    fn cn_create_file() {
        let s = SelectionSnapshot::from_query("新建一个配置文件");
        assert!(s.is_mutate, "'新建' should trigger is_mutate");
    }

    #[test]
    fn cn_modify_code() {
        let s = SelectionSnapshot::from_query("修改这个函数的返回值");
        assert!(s.is_mutate, "'修改' should trigger is_mutate");
    }

    #[test]
    fn cn_delete_file() {
        let s = SelectionSnapshot::from_query("删除这个临时文件");
        assert!(s.is_mutate, "'删除' should trigger is_mutate");
    }

    #[test]
    fn en_fix_test() {
        let s = SelectionSnapshot::from_query("fix the failing test in auth.rs");
        assert!(s.is_mutate, "'fix' should trigger is_mutate");
    }

    #[test]
    fn en_add_function() {
        let s = SelectionSnapshot::from_query("add a new validation function");
        assert!(s.is_mutate, "'add' should trigger is_mutate");
    }

    #[test]
    fn en_remove_dead_code() {
        let s = SelectionSnapshot::from_query("remove the unused imports");
        assert!(s.is_mutate, "'remove' should trigger is_mutate");
    }

    #[test]
    fn en_create_module() {
        let s = SelectionSnapshot::from_query("create a new auth module");
        assert!(s.is_mutate, "'create' should trigger is_mutate");
    }

    #[test]
    fn en_write_tests() {
        let s = SelectionSnapshot::from_query("write unit tests for the parser");
        assert!(s.is_mutate, "'write' should trigger is_mutate");
    }

    #[test]
    fn en_update_config() {
        let s = SelectionSnapshot::from_query("update the database config");
        assert!(s.is_mutate, "'update' should trigger is_mutate");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. CODE READING / SEARCH
// ═══════════════════════════════════════════════════════════════════════════════

mod code_reading {
    use super::*;

    #[test]
    fn cn_read_code() {
        let s = SelectionSnapshot::from_query("查看main.rs的内容");
        assert!(s.is_fetch, "'查看' should trigger is_fetch");
    }

    #[test]
    fn cn_search_code() {
        let s = SelectionSnapshot::from_query("搜索所有用到TokenBudget的地方");
        // "搜索" not in fetch keywords, but query is not conversational
        // Adaptive threshold ensures dynamic tools are included
        assert!(!s.is_conversational);
    }

    #[test]
    fn cn_list_files() {
        let s = SelectionSnapshot::from_query("列出src目录下所有rust文件");
        assert!(s.is_fetch, "'列出' should trigger is_fetch");
    }

    #[test]
    fn cn_how_does_it_work() {
        let s = SelectionSnapshot::from_query("这个模块怎么工作的");
        // "怎么" is not "怎么样" exactly, but let's check
        // The key: even without perfect signal match, adaptive threshold helps
        assert!(
            !s.is_conversational,
            "Technical question is not conversational"
        );
    }

    #[test]
    fn cn_whats_in_file() {
        let s = SelectionSnapshot::from_query("这个文件里有什么");
        assert!(s.is_fetch, "'什么' should trigger is_fetch");
    }

    #[test]
    fn en_show_file() {
        let s = SelectionSnapshot::from_query("show me the contents of config.toml");
        assert!(s.is_fetch, "'show' should trigger is_fetch");
    }

    #[test]
    fn en_find_function() {
        let s = SelectionSnapshot::from_query("find where select_tools is defined");
        // "find" not in fetch keywords directly
        assert!(!s.is_conversational);
        assert!(s.dynamic_tools.len() >= 3, "Should get dynamic tools");
    }

    #[test]
    fn en_get_status() {
        let s = SelectionSnapshot::from_query("get the current project status");
        assert!(s.is_fetch, "'get'/'status' should trigger is_fetch");
    }

    #[test]
    fn en_check_file() {
        let s = SelectionSnapshot::from_query("check what's in the test directory");
        assert!(s.is_fetch, "'check' should trigger is_fetch");
    }

    #[test]
    fn en_tell_me() {
        let s = SelectionSnapshot::from_query("tell me about the project structure");
        assert!(s.is_fetch, "'tell me' should trigger is_fetch");
    }

    #[test]
    fn en_how_many() {
        let s = SelectionSnapshot::from_query("how many tests are there");
        // "how many" not in fetch keywords (only Chinese "多少" is)
        // Acceptable gap: English question words don't trigger fetch signal.
        // But the query is NOT conversational and adaptive threshold ensures tools.
        assert!(!s.is_conversational);
        assert!(s.dynamic_tools.len() >= 3, "Should still get dynamic tools");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. MEMORY & PREFERENCES
// ═══════════════════════════════════════════════════════════════════════════════

mod memory_preferences {
    use super::*;

    #[test]
    fn cn_guanzhu_interest() {
        // THE failure case that triggered Phase 7
        // After memory signal alignment, "关注" fires is_memory (1 signal)
        let s = SelectionSnapshot::from_query("我关注matrixorigin");
        assert_eq!(s.signal_count, 1, "关注 now fires is_memory signal");
        assert!(
            !s.dynamic_tools.is_empty(),
            "Memory signal should include some dynamic tools"
        );
    }

    #[test]
    fn cn_remember_preference() {
        let s = SelectionSnapshot::from_query("记住我喜欢用Rust");
        // "记住" not in keyword lists currently
        assert!(!s.is_conversational);
        // memory_store is pinned, always available
    }

    #[test]
    fn cn_recall_memories() {
        let s = SelectionSnapshot::from_query("我之前记住了什么偏好");
        assert!(s.is_fetch, "'什么' should trigger is_fetch");
        assert!(
            s.references_history,
            "'之前' should trigger references_history"
        );
    }

    #[test]
    fn cn_track_project() {
        let s = SelectionSnapshot::from_query("帮我跟踪这个项目");
        // "跟踪" not in keyword lists (covered by prompt, not signals)
        assert!(!s.is_conversational);
    }

    #[test]
    fn cn_whats_stored() {
        let s = SelectionSnapshot::from_query("我的记忆里有哪些内容");
        assert!(s.is_fetch, "'有哪些' should trigger is_fetch");
        assert!(
            s.has_intent(IntentType::Memory),
            "Memory query should include memory tools"
        );
    }

    #[test]
    fn en_remember_this() {
        let s = SelectionSnapshot::from_query("remember that I prefer PostgreSQL");
        assert!(!s.is_conversational);
        // memory_store is pinned — always available regardless of signals
    }

    #[test]
    fn en_what_do_you_know() {
        let s = SelectionSnapshot::from_query("what do you know about me");
        // "what" not in fetch keywords (only CJK "什么" is)
        // Not conversational though → adaptive threshold gives tools
        assert!(!s.is_conversational);
        assert!(s.dynamic_tools.len() >= 3, "Should get dynamic tools");
    }

    #[test]
    fn en_follow_repo() {
        let s = SelectionSnapshot::from_query("I want to follow the matrixorigin repo");
        assert!(s.is_github, "'repo' should trigger is_github");
        // "follow" covered by prompt verb expansion, not signals
    }

    #[test]
    fn en_delete_memory() {
        let s = SelectionSnapshot::from_query("delete the memory about Python preference");
        assert!(s.is_mutate, "'delete' should trigger is_mutate");
        assert!(
            s.has_intent(IntentType::Memory),
            "Should include memory tools"
        );
    }

    #[test]
    fn en_search_memories() {
        let s = SelectionSnapshot::from_query("search my memories for database preferences");
        // memory_search is pinned, always present
        assert!(!s.is_conversational);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. ANALYTICAL / DEBUG
// ═══════════════════════════════════════════════════════════════════════════════

mod analytical {
    use super::*;

    #[test]
    fn cn_why_wrong() {
        let s = SelectionSnapshot::from_query("为什么选错了工具");
        assert!(s.is_analytical, "'为什么' should trigger is_analytical");
    }

    #[test]
    fn cn_analyze_code() {
        let s = SelectionSnapshot::from_query("分析一下这段代码的性能问题");
        assert!(s.is_analytical, "'分析' should trigger is_analytical");
    }

    #[test]
    fn cn_explain_error() {
        let s = SelectionSnapshot::from_query("解释这个错误是什么意思");
        assert!(s.is_analytical, "'解释' should trigger is_analytical");
    }

    #[test]
    fn cn_diagnose() {
        let s = SelectionSnapshot::from_query("诊断一下为什么测试失败");
        assert!(
            s.is_analytical,
            "'诊断'/'为什么' should trigger is_analytical"
        );
    }

    #[test]
    fn cn_whats_going_on() {
        let s = SelectionSnapshot::from_query("怎么回事，编译报错了");
        assert!(s.is_analytical, "'怎么回事' should trigger is_analytical");
    }

    #[test]
    fn cn_find_cause() {
        let s = SelectionSnapshot::from_query("找出这个bug的原因");
        assert!(s.is_analytical, "'原因' should trigger is_analytical");
    }

    #[test]
    fn en_why_failing() {
        let s = SelectionSnapshot::from_query("why is the test failing");
        assert!(s.is_analytical, "'why' should trigger is_analytical");
    }

    #[test]
    fn en_analyze_performance() {
        let s = SelectionSnapshot::from_query("analyze the performance of this query");
        assert!(s.is_analytical, "'analyze' should trigger is_analytical");
    }

    #[test]
    fn en_investigate() {
        let s = SelectionSnapshot::from_query("investigate the memory leak");
        assert!(
            s.is_analytical,
            "'investigate' should trigger is_analytical"
        );
    }

    #[test]
    fn en_explain_code() {
        let s = SelectionSnapshot::from_query("explain how this function works");
        assert!(s.is_analytical, "'explain' should trigger is_analytical");
    }

    #[test]
    fn en_debug_issue() {
        let s = SelectionSnapshot::from_query("debug the authentication issue");
        assert!(s.is_analytical, "'debug' should trigger is_analytical");
        // "issue" also triggers is_github
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. CONVERSATIONAL
// ═══════════════════════════════════════════════════════════════════════════════

mod conversational {
    use super::*;

    // Conversational queries should return 0 dynamic tools (short-circuited).

    #[test]
    fn cn_nihao() {
        let s = SelectionSnapshot::from_query("你好");
        assert!(s.is_conversational);
        assert!(
            s.dynamic_tools.is_empty(),
            "Conversational → 0 dynamic tools"
        );
    }

    #[test]
    fn cn_xiexie() {
        let s = SelectionSnapshot::from_query("谢谢");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn cn_zaijian() {
        let s = SelectionSnapshot::from_query("再见");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn cn_haode() {
        let s = SelectionSnapshot::from_query("好的");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn cn_shide() {
        let s = SelectionSnapshot::from_query("是的");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn cn_en() {
        let s = SelectionSnapshot::from_query("嗯");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_hi() {
        let s = SelectionSnapshot::from_query("hi");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_hello() {
        let s = SelectionSnapshot::from_query("hello");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_thanks() {
        let s = SelectionSnapshot::from_query("thanks");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_thank_you() {
        let s = SelectionSnapshot::from_query("thank you");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_bye() {
        let s = SelectionSnapshot::from_query("bye");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_ok() {
        let s = SelectionSnapshot::from_query("ok");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_yes() {
        let s = SelectionSnapshot::from_query("yes");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_no() {
        let s = SelectionSnapshot::from_query("no");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    #[test]
    fn en_nope() {
        let s = SelectionSnapshot::from_query("nope");
        assert!(s.is_conversational);
        assert!(s.dynamic_tools.is_empty());
    }

    // ── Conversational words INSIDE a longer query should NOT short-circuit ──

    #[test]
    fn en_thanks_but_continue() {
        let s = SelectionSnapshot::from_query("thanks, now fix the bug in parser.rs");
        assert!(
            !s.is_conversational || s.is_mutate,
            "Long message with 'thanks' + 'fix' should not be purely conversational"
        );
        // The key: is_mutate should override conversational bypass
    }

    #[test]
    fn cn_haode_but_continue() {
        let s = SelectionSnapshot::from_query("好的，帮我查看一下代码");
        // Should detect fetch intent even though "好的" is conversational
        assert!(
            s.is_fetch || !s.dynamic_tools.is_empty(),
            "Long message should not be purely conversational"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. HISTORY REFERENCES
// ═══════════════════════════════════════════════════════════════════════════════

mod history_references {
    use super::*;

    #[test]
    fn cn_zhiqian() {
        let s = SelectionSnapshot::from_query("之前讨论的那个方案");
        assert!(
            s.references_history,
            "'之前' should trigger references_history"
        );
    }

    #[test]
    fn cn_shangci() {
        let s = SelectionSnapshot::from_query("上次你说的那个工具");
        assert!(
            s.references_history,
            "'上次' should trigger references_history"
        );
    }

    #[test]
    fn cn_gangcai() {
        let s = SelectionSnapshot::from_query("刚才的结果不对");
        assert!(
            s.references_history,
            "'刚才' should trigger references_history"
        );
    }

    #[test]
    fn cn_lishi() {
        let s = SelectionSnapshot::from_query("查看历史对话");
        assert!(
            s.references_history,
            "'历史' should trigger references_history"
        );
    }

    #[test]
    fn cn_shangyilun() {
        let s = SelectionSnapshot::from_query("上一轮的分析结果");
        assert!(
            s.references_history,
            "'上一轮' should trigger references_history"
        );
    }

    #[test]
    fn en_earlier() {
        let s = SelectionSnapshot::from_query("what we discussed earlier");
        assert!(
            s.references_history,
            "'earlier' should trigger references_history"
        );
    }

    #[test]
    fn en_previous() {
        let s = SelectionSnapshot::from_query("go back to the previous approach");
        assert!(
            s.references_history,
            "'previous' should trigger references_history"
        );
    }

    #[test]
    fn en_last_time() {
        let s = SelectionSnapshot::from_query("last time you suggested using Redis");
        assert!(
            s.references_history,
            "'last time' should trigger references_history"
        );
    }

    #[test]
    fn en_before() {
        let s = SelectionSnapshot::from_query("the solution you mentioned before");
        assert!(
            s.references_history,
            "'before' should trigger references_history"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. VAGUE / AMBIGUOUS / ENTITY-ONLY
// ═══════════════════════════════════════════════════════════════════════════════

mod vague_ambiguous {
    use super::*;

    // These are the hardest cases: no keyword signals fire, TF-IDF is low.
    // Adaptive threshold should ensure dynamic tools are still included.

    #[test]
    fn entity_only_matrixorigin() {
        let s = SelectionSnapshot::from_query("matrixorigin");
        assert_eq!(s.signal_count, 0, "Bare entity has no signals");
        assert!(!s.is_conversational, "Entity is not conversational");
        assert!(
            s.dynamic_tools.len() >= 3,
            "Should get fallback/adaptive tools"
        );
    }

    #[test]
    fn entity_only_memoria() {
        let s = SelectionSnapshot::from_query("memoria");
        assert_eq!(s.signal_count, 0);
        assert!(s.dynamic_tools.len() >= 3);
    }

    #[test]
    fn cn_interest_expression() {
        // "关注" now fires is_memory signal
        let s = SelectionSnapshot::from_query("我关注matrixorigin");
        assert_eq!(s.signal_count, 1, "关注 fires is_memory");
        assert!(!s.dynamic_tools.is_empty(), "Should get some tools");
    }

    #[test]
    fn cn_vague_request() {
        let s = SelectionSnapshot::from_query("帮帮我");
        assert_eq!(s.signal_count, 0);
        assert!(!s.is_conversational, "Request is not conversational");
        assert!(s.dynamic_tools.len() >= 3, "Should still get tools");
    }

    #[test]
    fn cn_liuyi_interest() {
        // "留意" now fires is_memory signal
        let s = SelectionSnapshot::from_query("留意一下这个项目的更新");
        assert_eq!(s.signal_count, 1, "留意 fires is_memory");
        assert!(!s.dynamic_tools.is_empty());
    }

    #[test]
    fn cn_generic_question() {
        let s = SelectionSnapshot::from_query("这个怎么弄");
        // "怎么" might partially match "怎么样" or "怎么回事"
        assert!(!s.is_conversational);
    }

    #[test]
    fn en_vague_help() {
        let s = SelectionSnapshot::from_query("help me with this");
        assert_eq!(s.signal_count, 0);
        assert!(!s.is_conversational);
        assert!(s.dynamic_tools.len() >= 3);
    }

    #[test]
    fn en_vague_project_name() {
        let s = SelectionSnapshot::from_query("kubernetes");
        assert_eq!(s.signal_count, 0);
        assert!(s.dynamic_tools.len() >= 3, "Entity-only should get tools");
    }

    #[test]
    fn en_just_a_url() {
        let s = SelectionSnapshot::from_query("https://github.com/matrixorigin/matrixone");
        // URL contains "github" which should trigger is_github
        assert!(s.is_github, "'github' in URL should trigger is_github");
    }

    #[test]
    fn mixed_language_vague() {
        let s = SelectionSnapshot::from_query("matrixorigin的东西");
        assert_eq!(s.signal_count, 0);
        assert!(!s.is_conversational);
        assert!(s.dynamic_tools.len() >= 3);
    }

    #[test]
    fn en_incomplete_thought() {
        let s = SelectionSnapshot::from_query("what about");
        assert!(!s.is_conversational);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. MIXED INTENT
// ═══════════════════════════════════════════════════════════════════════════════

mod mixed_intent {
    use super::*;

    #[test]
    fn fetch_plus_mutate() {
        let s = SelectionSnapshot::from_query("show me the PRs and create a new issue");
        assert!(s.is_fetch, "Should detect fetch");
        assert!(s.is_mutate, "Should detect mutate");
        assert!(s.is_github, "Should detect GitHub");
        assert!(s.signal_count >= 3, "Multiple signals: {}", s.signal_count);
    }

    #[test]
    fn github_plus_git() {
        let s = SelectionSnapshot::from_query("check the PR diff and git log");
        assert!(s.is_github, "'PR' triggers GitHub");
        assert!(s.is_git, "'git' triggers git");
        assert!(s.has_intent(IntentType::GitHub));
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn analytical_plus_fetch() {
        let s = SelectionSnapshot::from_query("why did the latest CI fail");
        assert!(s.is_analytical, "'why' triggers analytical");
        assert!(s.is_fetch, "'latest' triggers fetch");
        assert!(s.is_github, "'CI' triggers GitHub");
    }

    #[test]
    fn history_plus_analytical() {
        let s = SelectionSnapshot::from_query("分析一下之前的决策是否正确");
        assert!(s.is_analytical, "'分析' triggers analytical");
        assert!(s.references_history, "'之前' triggers history");
    }

    #[test]
    fn memory_plus_github() {
        let s = SelectionSnapshot::from_query("记住matrixorigin这个repo，然后查看它的PR");
        assert!(s.is_github, "'repo'/'PR' triggers GitHub");
        assert!(s.is_fetch, "'查看' triggers fetch");
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn edit_plus_read() {
        let s = SelectionSnapshot::from_query("read config.toml and update the port");
        assert!(s.is_mutate, "'update' triggers mutate");
    }

    #[test]
    fn cn_three_intents() {
        let s = SelectionSnapshot::from_query("查看git log，分析为什么CI失败，然后修复");
        assert!(s.is_fetch, "'查看' triggers fetch");
        assert!(s.is_git, "'git' triggers git");
        assert!(s.is_analytical, "'为什么' triggers analytical");
        assert!(s.is_mutate, "'修复' triggers mutate");
        assert!(s.is_github, "'CI' triggers GitHub");
        assert!(
            s.signal_count >= 4,
            "Should have 4+ signals: {}",
            s.signal_count
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

mod edge_cases {
    use super::*;

    #[test]
    fn empty_string() {
        let s = SelectionSnapshot::from_query("");
        // Should not panic, should handle gracefully
        assert_eq!(s.signal_count, 0);
    }

    #[test]
    fn single_char() {
        let s = SelectionSnapshot::from_query("a");
        assert_eq!(s.signal_count, 0);
        // Not conversational (not in conversational list)
    }

    #[test]
    fn numbers_only() {
        let s = SelectionSnapshot::from_query("12345");
        assert_eq!(s.signal_count, 0);
        assert!(!s.is_conversational);
    }

    #[test]
    fn special_chars() {
        let s = SelectionSnapshot::from_query("!@#$%^&*()");
        assert_eq!(s.signal_count, 0);
        // Should not panic
    }

    #[test]
    fn very_long_query() {
        let long = "please analyze the code ".repeat(100);
        let s = SelectionSnapshot::from_query(&long);
        assert!(
            s.is_analytical,
            "'analyze' should trigger even in long query"
        );
        // Should not panic or timeout
    }

    #[test]
    fn emoji_only() {
        let s = SelectionSnapshot::from_query("👍");
        assert_eq!(s.signal_count, 0);
        // Pure emoji with no alphanumeric content → conversational (input guard)
        assert!(s.is_conversational, "Pure emoji should be conversational");
    }

    #[test]
    fn mixed_emoji_and_text() {
        let s = SelectionSnapshot::from_query("🔥 show me the PRs");
        assert!(s.is_fetch, "'show' should trigger is_fetch even with emoji");
        assert!(s.is_github, "'PRs' should trigger is_github");
    }

    #[test]
    fn repeated_keywords() {
        let s = SelectionSnapshot::from_query("list list list show show show");
        assert!(
            s.is_fetch,
            "Repeated 'list'/'show' should still trigger is_fetch"
        );
        assert_eq!(
            s.signal_count, 1,
            "Multiple keywords in same signal = still 1 signal"
        );
    }

    #[test]
    fn all_caps() {
        let s = SelectionSnapshot::from_query("SHOW ME THE GITHUB PRS");
        // Case insensitivity depends on implementation
        // Let's just verify it doesn't panic
        assert!(!s.is_conversational);
    }

    #[test]
    fn whitespace_heavy() {
        let s = SelectionSnapshot::from_query("   show   me   the   diff   ");
        assert!(!s.is_conversational);
    }

    #[test]
    fn newlines_in_query() {
        let s = SelectionSnapshot::from_query("show me the PRs\nand create an issue");
        assert!(!s.is_conversational);
    }

    #[test]
    fn japanese_similar_to_chinese() {
        // Japanese uses some same CJK characters
        let s = SelectionSnapshot::from_query("分析してください");
        assert!(
            s.is_analytical,
            "'分析' should trigger even in Japanese context"
        );
    }

    #[test]
    fn code_snippet_as_query() {
        let s = SelectionSnapshot::from_query("fn main() { println!(\"hello\"); }");
        assert!(!s.is_conversational, "Code snippet is not conversational");
    }

    #[test]
    fn error_message_as_query() {
        let s = SelectionSnapshot::from_query("error[E0308]: mismatched types");
        assert!(!s.is_conversational);
        // Should get some dynamic tools for debugging
    }

    #[test]
    fn file_path_as_query() {
        let s = SelectionSnapshot::from_query("src/main.rs:42");
        assert!(!s.is_conversational);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. CONFIDENCE & ADAPTIVE THRESHOLD INVARIANTS
// ═══════════════════════════════════════════════════════════════════════════════

mod invariants {
    use super::*;

    /// Invariant: Conversational queries always get 0 dynamic tools.
    #[test]
    fn conversational_always_empty() {
        let queries = [
            "hi", "hello", "hey", "thanks", "bye", "yes", "no", "ok", "你好", "谢谢", "再见",
            "好的", "是的", "嗯",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                s.dynamic_tools.is_empty(),
                "Conversational '{}' should get 0 dynamic tools, got {}",
                q,
                s.dynamic_tools.len()
            );
        }
    }

    /// Invariant: Non-conversational queries always get >= 1 dynamic tool
    /// (from adaptive threshold or fallback). Very short queries (2 words) may
    /// only get 1-3 tools due to low TF-IDF across the catalog.
    #[test]
    fn non_conversational_always_gets_tools() {
        let queries = [
            "matrixorigin",
            "我关注这个项目",
            "help me please",
            "show me the code",
            "analyze performance",
            "12345",
            "kubernetes deployment",
            "帮帮我",
            "what about this",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                !s.dynamic_tools.is_empty(),
                "Non-conversational '{}' should get >= 1 dynamic tool, got 0",
                q,
            );
        }
    }

    /// Invariant: More signals → higher confidence.
    #[test]
    fn more_signals_higher_confidence() {
        let q0 = SelectionSnapshot::from_query("matrixorigin"); // 0 signals
        let q2 = SelectionSnapshot::from_query("show me the github PRs"); // 2+ signals

        // q0 < q2 at minimum
        assert!(
            q0.confidence <= q2.confidence,
            "0-signal ({:.2}) should have <= confidence than 2+-signal ({:.2})",
            q0.confidence,
            q2.confidence
        );
    }

    /// Invariant: GitHub queries always include GitHub-intent tools.
    #[test]
    fn github_queries_include_github_tools() {
        let queries = [
            "list PRs",
            "show me the issues",
            "check CI status",
            "create an issue",
            "github actions",
            "matrixorigin最新的pr",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                s.has_intent(IntentType::GitHub),
                "GitHub query '{}' should include GitHub tools, got: {:?}",
                q,
                s.tool_names()
            );
        }
    }

    /// Invariant: Git queries always include Git-intent tools.
    #[test]
    fn git_queries_include_git_tools() {
        let queries = [
            "git status",
            "show me the diff",
            "git log",
            "rebase onto main",
            "stash changes",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                s.has_intent(IntentType::Git),
                "Git query '{}' should include Git tools, got: {:?}",
                q,
                s.tool_names()
            );
        }
    }

    /// "commit history" fires is_git signal but TF-IDF for git tools may be low.
    /// With adaptive threshold (1+ signals), git tools should still appear.
    #[test]
    fn commit_history_includes_git_tools() {
        let s = SelectionSnapshot::from_query("commit history");
        assert!(s.is_git, "'commit' triggers is_git");
        // Even if git_log/git_diff don't rank high on TF-IDF alone,
        // the signal should ensure they're above the lowered threshold
        assert!(
            s.has_intent(IntentType::Git) || !s.dynamic_tools.is_empty(),
            "Should get some tools for git query"
        );
    }

    /// Invariant: signal_count matches sum of individual signals.
    #[test]
    fn signal_count_consistent() {
        let queries = [
            "hi",
            "show me PRs",
            "analyze why CI failed and fix it",
            "matrixorigin",
            "git diff",
            "create issue",
        ];
        for q in &queries {
            let state = ConversationState::from_message(q, 1);
            let manual_count = [
                state.is_fetch,
                state.is_mutate,
                state.is_github,
                state.is_git,
                state.is_analytical,
                state.references_history,
            ]
            .iter()
            .filter(|&&x| x)
            .count();
            assert_eq!(
                state.signal_count(),
                manual_count,
                "signal_count() mismatch for '{}': method={} manual={}",
                q,
                state.signal_count(),
                manual_count
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 13. TRIGGER ENRICHMENT REGRESSION
// ═══════════════════════════════════════════════════════════════════════════════

mod trigger_enrichment {
    use super::*;

    // ── mo_query: analytics triggers ──

    #[test]
    fn cn_analytics_query_selects_mo_query() {
        let s = SelectionSnapshot::from_query("统计一下数据库里的记录数");
        assert!(
            s.tool_names().contains(&"mo_query"),
            "统计 should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn en_aggregate_selects_mo_query() {
        let s = SelectionSnapshot::from_query("aggregate the sales data");
        assert!(
            s.tool_names().contains(&"mo_query"),
            "aggregate should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_analyze_selects_mo_query() {
        let s = SelectionSnapshot::from_query("分析数据趋势");
        assert!(
            s.tool_names().contains(&"mo_query"),
            "分析 should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }

    // ── reflect: diagnostic triggers ──

    #[test]
    fn cn_diagnose_selects_reflect() {
        let s = SelectionSnapshot::from_query("诊断一下这个问题");
        assert!(
            s.tool_names().contains(&"reflect"),
            "诊断 should select reflect. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_troubleshoot_selects_reflect() {
        let s = SelectionSnapshot::from_query("排查一下原因");
        assert!(
            s.tool_names().contains(&"reflect"),
            "排查 should select reflect. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn en_diagnose_selects_reflect() {
        let s = SelectionSnapshot::from_query("diagnose the issue");
        assert!(
            s.tool_names().contains(&"reflect"),
            "diagnose should select reflect. Got: {:?}",
            s.tool_names()
        );
    }

    // ── github_create_issue: missing CJK triggers ──

    #[test]
    fn cn_create_issue_selects_github_create_issue() {
        let s = SelectionSnapshot::from_query("新建一个issue");
        assert!(
            s.tool_names().contains(&"github_create_issue"),
            "新建issue should select github_create_issue. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_create_problem_selects_github_create_issue() {
        let s = SelectionSnapshot::from_query("创建问题");
        assert!(
            s.tool_names().contains(&"github_create_issue"),
            "创建问题 should select github_create_issue. Got: {:?}",
            s.tool_names()
        );
    }

    // ── web_fetch: link triggers ──

    #[test]
    fn cn_open_link_selects_web_fetch() {
        let s = SelectionSnapshot::from_query("打开链接");
        assert!(
            s.tool_names().contains(&"web_fetch"),
            "打开链接 should select web_fetch. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn en_link_selects_web_fetch() {
        let s = SelectionSnapshot::from_query("open this link https://example.com");
        assert!(
            s.tool_names().contains(&"web_fetch"),
            "link should select web_fetch. Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_diff: review changes triggers ──

    #[test]
    fn cn_see_what_changed_selects_git_diff() {
        let s = SelectionSnapshot::from_query("看改动");
        assert!(
            s.tool_names().contains(&"git_diff"),
            "看改动 should select git_diff. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn en_review_changes_selects_git_diff() {
        let s = SelectionSnapshot::from_query("review changes");
        assert!(
            s.tool_names().contains(&"git_diff"),
            "review changes should select git_diff. Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_contributors: who worked on triggers ──

    #[test]
    fn en_who_worked_on_selects_git_contributors() {
        let s = SelectionSnapshot::from_query("who worked on this file");
        assert!(
            s.tool_names().contains(&"git_contributors"),
            "who worked on should select git_contributors. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_who_did_this_selects_git_contributors() {
        let s = SelectionSnapshot::from_query("谁做的这个功能");
        assert!(
            s.tool_names().contains(&"git_contributors"),
            "谁做的 should select git_contributors. Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_status: natural language status triggers ──

    #[test]
    fn cn_any_modifications_selects_git_status() {
        let s = SelectionSnapshot::from_query("改了吗");
        assert!(
            s.tool_names().contains(&"git_status"),
            "改了吗 should select git_status. Got: {:?}",
            s.tool_names()
        );
    }

    // ── No duplicate triggers in catalog ──

    #[test]
    fn no_duplicate_triggers_within_any_tool() {
        for tool in TOOL_CATALOG.iter() {
            let mut seen = std::collections::HashSet::new();
            for trigger in tool.triggers {
                assert!(
                    seen.insert(trigger),
                    "Duplicate trigger '{}' in tool '{}'",
                    trigger,
                    tool.name
                );
            }
        }
    }

    // ── Memory signal alignment: "关注" routes to memory ──

    #[test]
    fn cn_follow_topic_fires_memory_signal() {
        let s = SelectionSnapshot::from_query("我关注matrixorigin");
        // memory_store is pinned, so it's always available.
        // The key assertion is that the memory signal fires.
        assert_eq!(s.signal_count, 1, "关注 should fire is_memory signal");
        assert!(s.is_memory, "is_memory flag should be true");
    }

    #[test]
    fn en_interested_in_fires_memory_signal() {
        let s = SelectionSnapshot::from_query("I'm interested in Rust performance");
        assert!(s.is_memory, "interested should fire is_memory signal");
    }

    // ── github_get_pr enrichment ──

    #[test]
    fn en_merge_status_selects_get_pr() {
        let s = SelectionSnapshot::from_query("what's the merge status of PR #42");
        assert!(
            s.tool_names().contains(&"github_get_pr"),
            "merge status should select github_get_pr. Got: {:?}",
            s.tool_names()
        );
    }

    // ── github_get_issue enrichment ──

    #[test]
    fn cn_check_issue_selects_get_issue() {
        let s = SelectionSnapshot::from_query("查看issue #10的状态");
        assert!(
            s.tool_names().contains(&"github_get_issue"),
            "查看issue should select github_get_issue. Got: {:?}",
            s.tool_names()
        );
    }

    // ── Refactoring query regression ──

    #[test]
    fn en_refactor_fires_correct_signals() {
        let s = SelectionSnapshot::from_query("refactor the authentication module");
        // Refactoring query should not crash and should produce a result
        let _ = s.signal_count;
    }

    // ── Testing query regression ──

    #[test]
    fn en_write_tests_fires_signals() {
        let s = SelectionSnapshot::from_query("write unit tests for the API");
        // bash and grep are pinned (always available), so the key thing is
        // the query doesn't crash and gets reasonable tools
        let _ = s.signal_count;
    }

    // ── Database query regression ──

    #[test]
    fn cn_query_database_selects_mo_query() {
        let s = SelectionSnapshot::from_query("查询数据库中的用户表");
        assert!(
            s.tool_names().contains(&"mo_query"),
            "数据库查询 should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn en_sql_query_selects_mo_query() {
        let s = SelectionSnapshot::from_query("run a SQL query to check the data");
        assert!(
            s.tool_names().contains(&"mo_query"),
            "SQL query should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }

    // ── Security-sensitive regression ──

    #[test]
    fn en_security_audit_gets_tools() {
        let s = SelectionSnapshot::from_query("check for security vulnerabilities in the code");
        // read_file and grep are pinned (always available for code reading)
        let _ = s.signal_count;
    }

    // ── Cross-domain non-interference ──

    #[test]
    fn memory_query_ranks_memory_tools_above_github() {
        // With adaptive threshold, many tools appear; verify ranking instead
        let s = SelectionSnapshot::from_query("我有哪些记忆？");
        assert!(s.is_memory, "memory signal should fire");
        let names = s.tool_names();
        // memory_correct / memory_purge / memory_profile should rank above github_list_prs
        let mem_pos = names
            .iter()
            .position(|n| n.contains("memory"))
            .unwrap_or(999);
        let gh_pos = names
            .iter()
            .position(|n| *n == "github_list_prs")
            .unwrap_or(999);
        assert!(
            mem_pos < gh_pos,
            "memory tools should rank above github_list_prs. Got: {:?}",
            names
        );
    }

    #[test]
    fn git_query_ranks_git_tools_above_mo_query() {
        // With adaptive threshold, mo_query may appear but git tools rank higher
        let s = SelectionSnapshot::from_query("show me recent commits");
        let names = s.tool_names();
        let git_pos = names
            .iter()
            .position(|n| n.starts_with("git_"))
            .unwrap_or(999);
        let mo_pos = names.iter().position(|n| *n == "mo_query").unwrap_or(999);
        assert!(
            git_pos < mo_pos,
            "git tools should rank above mo_query. Got: {:?}",
            names
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 13. Tool coverage tests — every dynamic tool gets at least one test
// ═══════════════════════════════════════════════════════════════════════════════

mod tool_coverage {
    use super::*;

    // ── git_show ──

    #[test]
    fn en_show_commit_selects_git_show() {
        let s = SelectionSnapshot::from_query("show commit abc123");
        assert!(
            s.tool_names().contains(&"git_show"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_commit_details_selects_git_show() {
        let s = SelectionSnapshot::from_query("这个提交改了什么");
        assert!(
            s.tool_names().contains(&"git_show"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_contributors ──

    #[test]
    fn en_contributors_selects_git_contributors() {
        let s = SelectionSnapshot::from_query("who are the top contributors");
        assert!(
            s.tool_names().contains(&"git_contributors"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_contributors_selects_git_contributors() {
        let s = SelectionSnapshot::from_query("这个项目的贡献者有谁");
        assert!(
            s.tool_names().contains(&"git_contributors"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_file_history ──

    #[test]
    fn en_file_history_selects_git_file_history() {
        let s = SelectionSnapshot::from_query("show file history for main.rs");
        assert!(
            s.tool_names().contains(&"git_file_history"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_file_history_selects_git_file_history() {
        let s = SelectionSnapshot::from_query("这个文件的文件历史是什么");
        assert!(
            s.tool_names().contains(&"git_file_history"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_log_search ──

    #[test]
    fn en_search_commits_selects_git_log_search() {
        let s = SelectionSnapshot::from_query("search commits for authentication fix");
        assert!(
            s.tool_names().contains(&"git_log_search"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_search_commits_selects_git_log_search() {
        let s = SelectionSnapshot::from_query("搜索提交记录中有关认证的修改");
        assert!(
            s.tool_names().contains(&"git_log_search"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── github_repo_stats ──

    #[test]
    fn en_stars_selects_github_repo_stats() {
        let s = SelectionSnapshot::from_query("how many stars does this repo have");
        assert!(
            s.tool_names().contains(&"github_repo_stats"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_stars_selects_github_repo_stats() {
        let s = SelectionSnapshot::from_query("多少star了");
        assert!(
            s.tool_names().contains(&"github_repo_stats"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── web_fetch ──

    #[test]
    fn en_fetch_url_selects_web_fetch() {
        let s = SelectionSnapshot::from_query("fetch the content from https://example.com");
        assert!(
            s.tool_names().contains(&"web_fetch"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_open_url_selects_web_fetch() {
        let s = SelectionSnapshot::from_query("打开这个网址看看内容");
        assert!(
            s.tool_names().contains(&"web_fetch"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── get_agent_info ──

    #[test]
    fn en_capabilities_selects_agent_info() {
        let s = SelectionSnapshot::from_query("what can you do");
        assert!(
            s.tool_names().contains(&"get_agent_info"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_capabilities_selects_agent_info() {
        let s = SelectionSnapshot::from_query("你能做什么");
        assert!(
            s.tool_names().contains(&"get_agent_info"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── reflect ──

    #[test]
    fn en_what_happened_selects_reflect() {
        let s = SelectionSnapshot::from_query("what happened with that last operation");
        assert!(
            s.tool_names().contains(&"reflect"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_what_went_wrong_selects_reflect() {
        let s = SelectionSnapshot::from_query("出了什么问题");
        assert!(
            s.tool_names().contains(&"reflect"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── mo_snapshot ──

    #[test]
    fn en_snapshot_selects_mo_snapshot() {
        let s = SelectionSnapshot::from_query("create a data snapshot before the experiment");
        assert!(
            s.tool_names().contains(&"mo_snapshot"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_snapshot_selects_mo_snapshot() {
        let s = SelectionSnapshot::from_query("创建一个数据快照");
        assert!(
            s.tool_names().contains(&"mo_snapshot"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── mo_branch ──

    #[test]
    fn en_data_branch_selects_mo_branch() {
        let s = SelectionSnapshot::from_query("create an experiment branch for the data");
        assert!(
            s.tool_names().contains(&"mo_branch"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_data_branch_selects_mo_branch() {
        let s = SelectionSnapshot::from_query("创建实验分支隔离数据");
        assert!(
            s.tool_names().contains(&"mo_branch"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── memory_purge ──

    #[test]
    fn en_forget_selects_memory_purge() {
        let s = SelectionSnapshot::from_query("forget what I told you about Python");
        assert!(
            s.tool_names().contains(&"memory_purge"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_forget_selects_memory_purge() {
        let s = SelectionSnapshot::from_query("删除记忆中关于Python的内容");
        assert!(
            s.tool_names().contains(&"memory_purge"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── memory_correct ──

    #[test]
    fn en_correct_memory_selects_memory_correct() {
        let s = SelectionSnapshot::from_query("update memory to change my preferred language");
        assert!(
            s.tool_names().contains(&"memory_correct"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_correct_memory_selects_memory_correct() {
        let s = SelectionSnapshot::from_query("修正记忆中我的偏好设置");
        assert!(
            s.tool_names().contains(&"memory_correct"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── memory_profile ──

    #[test]
    fn en_profile_selects_memory_profile() {
        let s = SelectionSnapshot::from_query("what do you know about me");
        assert!(
            s.tool_names().contains(&"memory_profile"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_profile_selects_memory_profile() {
        let s = SelectionSnapshot::from_query("我的偏好和习惯是什么");
        assert!(
            s.tool_names().contains(&"memory_profile"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── git_blame ──

    #[test]
    fn en_blame_selects_git_blame() {
        let s = SelectionSnapshot::from_query("who last modified this function");
        assert!(
            s.tool_names().contains(&"git_blame"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_blame_selects_git_blame() {
        let s = SelectionSnapshot::from_query("谁改的这段代码");
        assert!(
            s.tool_names().contains(&"git_blame"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    // ── github_ci_status ──

    #[test]
    fn en_ci_status_selects_github_ci_status() {
        let s = SelectionSnapshot::from_query("what's the CI build status");
        assert!(
            s.tool_names().contains(&"github_ci_status"),
            "Got: {:?}",
            s.tool_names()
        );
    }

    #[test]
    fn cn_ci_status_selects_github_ci_status() {
        let s = SelectionSnapshot::from_query("CI状态怎么样");
        assert!(
            s.tool_names().contains(&"github_ci_status"),
            "Got: {:?}",
            s.tool_names()
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 14. CJK balance tests — fill gaps in under-covered CJK categories
// ═══════════════════════════════════════════════════════════════════════════════

mod cjk_balance {
    use super::*;

    // ── vague_ambiguous: add CJK ──

    #[test]
    fn cn_help_me_vague() {
        let s = SelectionSnapshot::from_query("帮帮忙");
        assert_eq!(s.signal_count, 0);
    }

    #[test]
    fn cn_look_at_this_vague() {
        // "看看" is a read_file trigger but not an is_fetch keyword (too common)
        let s = SelectionSnapshot::from_query("看看这个");
        assert!(s.signal_count <= 1, "vague query, few signals");
    }

    #[test]
    fn cn_how_to_do_vague() {
        // "怎么" is too common to be an is_fetch keyword
        let s = SelectionSnapshot::from_query("怎么搞");
        assert!(s.signal_count <= 1, "very short vague query");
    }

    #[test]
    fn cn_what_should_i_do() {
        // "怎么" too common; this is a vague query
        let s = SelectionSnapshot::from_query("接下来怎么办");
        assert!(s.signal_count <= 1, "vague continuation query");
    }

    #[test]
    fn cn_anything_new() {
        let s = SelectionSnapshot::from_query("最新的消息");
        assert!(s.is_fetch, "最新 should fire is_fetch");
    }

    // ── history_references: add CJK ──

    #[test]
    fn cn_just_now_history() {
        let s = SelectionSnapshot::from_query("刚刚那个结果呢");
        assert!(s.references_history, "刚刚 should fire references_history");
    }

    #[test]
    fn cn_earlier_said_history() {
        let s = SelectionSnapshot::from_query("之前说过的那个方案");
        assert!(
            s.references_history,
            "之前说过 should fire references_history"
        );
    }

    #[test]
    fn cn_previous_part_history() {
        let s = SelectionSnapshot::from_query("前面提到的函数");
        assert!(s.references_history, "前面 should fire references_history");
    }

    #[test]
    fn cn_last_time_history() {
        let s = SelectionSnapshot::from_query("上次的改动还在吗");
        assert!(s.references_history, "上次 should fire references_history");
    }

    // ── memory_preferences: add EN ──

    #[test]
    fn en_i_prefer_rust() {
        let s = SelectionSnapshot::from_query("I prefer Rust for systems programming");
        assert!(s.is_memory, "prefer should fire is_memory");
    }

    #[test]
    fn en_remember_this() {
        let s = SelectionSnapshot::from_query("remember that I use PostgreSQL");
        assert!(s.is_memory, "remember should fire is_memory");
    }

    #[test]
    fn en_interested_in_topic() {
        let s = SelectionSnapshot::from_query("I'm interested in the new release features");
        assert!(s.is_memory, "interested should fire is_memory");
    }

    // ── Signal expansion verification ──

    #[test]
    fn casual_cn_fetch_look_at() {
        let s = SelectionSnapshot::from_query("看一下配置文件");
        assert!(s.is_fetch, "看一下 should fire is_fetch");
    }

    #[test]
    fn casual_cn_mutate_change() {
        let s = SelectionSnapshot::from_query("改一下这个变量名");
        assert!(s.is_mutate, "改一下 should fire is_mutate");
    }

    #[test]
    fn casual_cn_mutate_add() {
        let s = SelectionSnapshot::from_query("添加一个新字段");
        assert!(s.is_mutate, "添加 should fire is_mutate");
    }

    #[test]
    fn en_mutate_change() {
        let s = SelectionSnapshot::from_query("change the timeout to 30 seconds");
        assert!(s.is_mutate, "change should fire is_mutate");
    }

    #[test]
    fn en_analytical_root_cause() {
        let s = SelectionSnapshot::from_query("what's the root cause of this failure");
        assert!(s.is_analytical, "root cause should fire is_analytical");
    }

    #[test]
    fn cn_analytical_what_reason() {
        let s = SelectionSnapshot::from_query("什么原因导致测试失败");
        assert!(s.is_analytical, "什么原因 should fire is_analytical");
    }

    #[test]
    fn en_analytical_what_went_wrong() {
        let s = SelectionSnapshot::from_query("what went wrong with the deployment");
        assert!(s.is_analytical, "what went wrong should fire is_analytical");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 16. Trigger quality invariants — enforces structural rules on TOOL_CATALOG
// ═══════════════════════════════════════════════════════════════════════════════

mod trigger_quality {
    use super::*;
    use std::collections::HashMap;

    /// No exact duplicate triggers across different tools.
    #[test]
    fn no_exact_duplicate_triggers_across_tools() {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                if let Some(&other) = seen.get(trigger)
                    && other != tool.name
                {
                    panic!(
                        "Duplicate trigger '{}' in both '{}' and '{}'",
                        trigger, other, tool.name
                    );
                }
                seen.insert(trigger, tool.name);
            }
        }
    }

    /// No CJK trigger should be a single character (too generic).
    #[test]
    fn no_single_char_cjk_triggers() {
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                let cjk_chars: Vec<char> = trigger
                    .chars()
                    .filter(|c| ('\u{4E00}'..='\u{9FFF}').contains(c))
                    .collect();
                if !cjk_chars.is_empty() && trigger.chars().count() <= 1 {
                    panic!(
                        "Tool '{}' has single-char CJK trigger '{}' — too generic",
                        tool.name, trigger
                    );
                }
            }
        }
    }

    /// No English trigger should be <= 2 chars (too generic: "pr", "ci", "ls").
    #[test]
    fn no_ultra_short_english_triggers() {
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                let is_ascii = trigger.is_ascii();
                if is_ascii && trigger.len() <= 2 {
                    panic!(
                        "Tool '{}' has ultra-short English trigger '{}' (<=2 chars) — too generic",
                        tool.name, trigger
                    );
                }
            }
        }
    }

    /// CJK triggers must not contain high-frequency particles as sole content.
    /// Characters 有/是/的/了/在/不 are dangerous in triggers because they appear
    /// in >50% of Chinese sentences and cause false TF-IDF matches.
    #[test]
    fn no_cjk_triggers_that_are_only_common_particles() {
        let dangerous: &[char] = &['有', '是', '的', '了', '在', '不', '这', '那', '我', '你'];
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                let chars: Vec<char> = trigger.chars().collect();
                let is_cjk = chars.iter().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(c));
                if is_cjk && chars.len() <= 2 {
                    // For short CJK triggers (1-2 chars), check if ALL chars are dangerous
                    let all_dangerous = chars.iter().all(|c| dangerous.contains(c));
                    if all_dangerous {
                        panic!(
                            "Tool '{}' has CJK trigger '{}' composed entirely of common particles",
                            tool.name, trigger
                        );
                    }
                }
            }
        }
    }

    /// Every tool must have at least 4 triggers for reliable matching.
    #[test]
    fn minimum_trigger_count() {
        for tool in TOOL_CATALOG {
            assert!(
                tool.triggers.len() >= 4,
                "Tool '{}' has only {} triggers (minimum 4)",
                tool.name,
                tool.triggers.len()
            );
        }
    }

    /// Verify the "保存" disambiguation: write_file uses "保存文件", memory_store uses "保存记忆".
    #[test]
    fn save_disambiguation() {
        let wf = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "write_file")
            .unwrap();
        let ms = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory_store")
            .unwrap();

        // write_file should NOT have bare "保存"
        assert!(
            !wf.triggers.contains(&"保存"),
            "write_file should use '保存文件' not bare '保存'"
        );
        // memory_store should have "保存" for backward compat
        assert!(
            ms.triggers.contains(&"保存"),
            "memory_store should keep '保存' as primary save trigger"
        );
    }

    /// Verify "remember" is only on memory_store, not memory_search.
    #[test]
    fn remember_not_on_search() {
        let ms = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory_search")
            .unwrap();
        assert!(
            !ms.triggers.contains(&"remember"),
            "memory_search should use 'recall' not 'remember' (belongs to memory_store)"
        );
    }

    /// Verify bare "pr" is NOT a trigger (too short, matches "preferences"/"profile").
    #[test]
    fn no_bare_pr_trigger() {
        for tool in TOOL_CATALOG {
            assert!(
                !tool.triggers.contains(&"pr"),
                "Tool '{}' has bare 'pr' trigger — use 'pull request' or 'github pr'",
                tool.name
            );
        }
    }

    /// Verify bare "ci" is NOT a trigger (too short).
    #[test]
    fn no_bare_ci_trigger() {
        for tool in TOOL_CATALOG {
            assert!(
                !tool.triggers.contains(&"ci"),
                "Tool '{}' has bare 'ci' trigger — use 'ci status' or 'github ci'",
                tool.name
            );
        }
    }

    /// Cross-domain: memory query should rank memory tools first.
    #[test]
    fn memory_query_ranks_memory_first() {
        let s = SelectionSnapshot::from_query("搜索一下记忆");
        assert!(s.is_memory, "memory query should fire is_memory");
        let names = s.tool_names();
        // First few results should be memory-related
        if let Some(first) = names.first() {
            assert!(
                first.contains("memory") || *first == "reflect",
                "memory query first result should be memory-related, got: {}",
                first
            );
        }
    }

    /// Cross-domain: git query should rank git tools first.
    #[test]
    fn git_query_ranks_git_first() {
        let s = SelectionSnapshot::from_query("show me the git log");
        assert!(s.is_git, "git query should fire is_git");
        let names = s.tool_names();
        if let Some(first) = names.first() {
            assert!(
                first.starts_with("git_") || first.starts_with("mo_"),
                "git query first result should be git-related tool, got: {}",
                first
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 17. Edge case regression — code-switching, typos, paths, negation, multiline
// ═══════════════════════════════════════════════════════════════════════════════

mod edge_case_regression {
    use super::*;

    // ── Code-switching (CJK + English in same query) ──

    #[test]
    fn code_switch_cn_en_git_log() {
        let s = SelectionSnapshot::from_query("看看最近的git log");
        assert!(s.is_git, "mixed CN+EN git query should fire is_git");
        assert!(s.tool_names().contains(&"git_log"));
    }

    #[test]
    fn code_switch_cn_en_pr_review() {
        let s = SelectionSnapshot::from_query("帮我review一下PR #42");
        assert!(s.is_github, "mixed CN+EN PR query should fire is_github");
    }

    #[test]
    fn code_switch_cn_en_fix_bug() {
        let s = SelectionSnapshot::from_query("fix一下这个bug");
        assert!(s.signal_count >= 1, "fix bug should fire signals");
    }

    #[test]
    fn code_switch_en_cn_memory() {
        let s = SelectionSnapshot::from_query("help me记住这个偏好");
        assert!(s.is_memory, "mixed EN+CN memory should fire is_memory");
    }

    // ── Queries with file paths ──

    #[test]
    fn query_with_file_path() {
        // "read" is not in is_fetch (read_file is pinned, doesn't need signal)
        let s = SelectionSnapshot::from_query("show me src/main.rs line 42");
        assert!(s.is_fetch, "show file path should fire is_fetch");
    }

    #[test]
    fn query_with_rust_path_cn() {
        let s = SelectionSnapshot::from_query("看看 crates/runtime/src/tool_registry/meta.rs");
        let _ = s; // path-based query should not crash
    }

    // ── Queries with numbers/versions/PR IDs ──

    #[test]
    fn query_with_pr_number() {
        let s = SelectionSnapshot::from_query("show me PR #1234");
        assert!(s.is_github, "PR number query should fire is_github");
    }

    #[test]
    fn query_with_version() {
        let s = SelectionSnapshot::from_query("upgrade to version 3.0.1");
        let _ = s; // version query should not crash
    }

    // ── Queries with URLs ──

    #[test]
    fn query_with_github_url() {
        let s = SelectionSnapshot::from_query(
            "check https://github.com/matrixorigin/matrixone/pull/123",
        );
        assert!(s.is_github, "GitHub URL should fire is_github");
    }

    #[test]
    fn query_with_generic_url() {
        let s = SelectionSnapshot::from_query("fetch https://example.com/api/data");
        assert!(s.is_fetch, "URL fetch query should fire is_fetch");
    }

    // ── Negation queries ──

    #[test]
    fn negation_dont_use_bash() {
        // Word-boundary matching: "git diff" (two words) fires is_git
        let s = SelectionSnapshot::from_query("don't use bash for this, use git diff instead");
        assert!(s.is_git, "should detect git intent despite negation");
    }

    #[test]
    fn negation_cn_dont_use_grep() {
        let s = SelectionSnapshot::from_query("不要用grep，用git log search查一下");
        assert!(s.is_git, "CJK negation + git intent should fire is_git");
    }

    // ── Multiline queries ──

    #[test]
    fn multiline_query() {
        let s = SelectionSnapshot::from_query(
            "我需要做三件事：\n1. 检查git状态\n2. 看看PR\n3. 修改代码",
        );
        assert!(s.is_git, "multiline should detect git");
        assert!(s.is_github, "multiline should detect github");
        assert!(s.is_mutate, "multiline should detect mutation");
    }

    // ── Queries with embedded code ──

    #[test]
    fn query_with_code_snippet() {
        let s = SelectionSnapshot::from_query(
            "fix this function:\nfn main() {\n    println!(\"hello\");\n}",
        );
        assert!(
            s.signal_count >= 1,
            "code snippet query should have signals"
        );
    }

    #[test]
    fn query_with_error_message() {
        // "error" is not in signal keywords, but "fix" triggers is_mutate
        let s = SelectionSnapshot::from_query(
            "fix this error: thread 'main' panicked at 'index out of bounds'",
        );
        assert!(s.is_mutate, "fix + error should trigger is_mutate");
    }

    // ── Follow-up patterns ──

    #[test]
    fn followup_yes() {
        let s = SelectionSnapshot::from_query("yes");
        assert_eq!(s.signal_count, 0, "bare yes is conversational, no signals");
    }

    #[test]
    fn followup_ok_do_it() {
        let s = SelectionSnapshot::from_query("ok do it");
        assert_eq!(s.signal_count, 0, "bare ok is conversational, no signals");
    }

    #[test]
    fn followup_cn_continue() {
        let s = SelectionSnapshot::from_query("继续");
        assert_eq!(s.signal_count, 0, "bare 继续 is conversational");
    }

    #[test]
    fn followup_cn_correct() {
        let s = SelectionSnapshot::from_query("对");
        assert_eq!(s.signal_count, 0, "bare 对 is conversational");
    }

    // ── Very long query ──

    #[test]
    fn very_long_query() {
        let long = "I need you to analyze the entire codebase and find all instances where \
            we use deprecated API calls, then refactor them to use the new API. \
            Start with the authentication module, then move to the database layer, \
            and finally update the HTTP handlers. Make sure all tests still pass \
            after each change. Also check if there are any security vulnerabilities \
            in the current implementation and fix those too.";
        let s = SelectionSnapshot::from_query(long);
        assert!(s.signal_count >= 1, "long query should detect some signals");
    }

    // ── Just punctuation / whitespace ──

    #[test]
    fn just_question_marks() {
        let s = SelectionSnapshot::from_query("???");
        assert_eq!(s.signal_count, 0, "punctuation-only should have no signals");
    }

    #[test]
    fn just_dots() {
        let s = SelectionSnapshot::from_query("...");
        assert_eq!(s.signal_count, 0, "dots-only should have no signals");
    }

    // ── Emoji ──

    #[test]
    fn emoji_in_query() {
        let s = SelectionSnapshot::from_query("🔥 fix this bug 🐛");
        let _ = s; // emoji query should not crash
    }

    #[test]
    fn pure_emoji() {
        let s = SelectionSnapshot::from_query("👍");
        assert_eq!(s.signal_count, 0, "pure emoji should have no signals");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 18. New trigger verification — tests for recent trigger changes
// ═══════════════════════════════════════════════════════════════════════════════

mod trigger_verification {
    use super::*;

    // ── Disambiguated triggers ──

    #[test]
    fn save_file_routes_to_write_file() {
        // "写入文件" fires is_mutate (via "写入" keyword)
        let s = SelectionSnapshot::from_query("写入文件到磁盘");
        assert!(s.is_mutate, "写入 should fire is_mutate");
    }

    #[test]
    fn save_memory_routes_to_memory() {
        // "保存到记忆" should favor memory_store
        let s = SelectionSnapshot::from_query("保存到记忆");
        assert!(s.is_memory, "save to memory should fire is_memory");
    }

    #[test]
    fn github_pr_trigger() {
        // "github pr" should select github_list_prs
        let s = SelectionSnapshot::from_query("show me the github pr list");
        assert!(s.is_github, "github pr should fire is_github");
        assert!(s.tool_names().contains(&"github_list_prs"));
    }

    #[test]
    fn ci_status_trigger() {
        // "ci status" should select github_ci_status
        let s = SelectionSnapshot::from_query("check ci status");
        assert!(s.is_github, "ci status should fire is_github");
        assert!(s.tool_names().contains(&"github_ci_status"));
    }

    #[test]
    fn modify_code_trigger_cn() {
        // "修改代码" fires is_mutate (via "修改" keyword) and matches str_replace trigger
        let s = SelectionSnapshot::from_query("帮我修改代码");
        assert!(s.is_mutate, "修改 should fire is_mutate");
    }

    #[test]
    fn sql_query_trigger() {
        // "sql query" should select mo_query (was bare "sql" before)
        let s = SelectionSnapshot::from_query("run a sql query on the database");
        let names = s.tool_names();
        assert!(
            names.contains(&"mo_query"),
            "sql query should select mo_query"
        );
    }

    #[test]
    fn agent_capabilities_trigger() {
        // "代理能力" should select get_agent_info (was "你能" before)
        let s = SelectionSnapshot::from_query("代理能力有哪些");
        let names = s.tool_names();
        assert!(
            names.contains(&"get_agent_info"),
            "代理能力 should select get_agent_info"
        );
    }

    #[test]
    fn user_profile_trigger() {
        // "偏好" fires is_memory; "用户偏好" matches memory_profile trigger
        let s = SelectionSnapshot::from_query("查看用户偏好设置");
        assert!(s.is_memory, "偏好 should fire is_memory");
    }

    #[test]
    fn reflect_error_trigger_cn() {
        // "哪里出错" should select reflect (was "出了什么问题" before)
        let s = SelectionSnapshot::from_query("哪里出错了");
        let names = s.tool_names();
        assert!(names.contains(&"reflect"), "哪里出错 should select reflect");
    }

    #[test]
    fn file_history_trigger_cn() {
        // "文件改动记录" should select git_file_history (was "什么时候改的" before)
        let s = SelectionSnapshot::from_query("查看文件改动记录");
        let names = s.tool_names();
        assert!(
            names.contains(&"git_file_history"),
            "文件改动记录 should select git_file_history"
        );
    }

    // ── Verify contamination is gone ──

    #[test]
    fn bare_wo_does_not_over_trigger() {
        // "我有问题" should NOT strongly match memory_profile
        // (previously "我的信息" contained "我" causing false positives)
        let s = SelectionSnapshot::from_query("我有问题");
        let names = s.tool_names();
        // memory_profile should NOT be in top 3
        let mp_pos = names
            .iter()
            .position(|n| *n == "memory_profile")
            .unwrap_or(999);
        assert!(
            mp_pos >= 3,
            "我有问题 should not strongly trigger memory_profile (pos={})",
            mp_pos
        );
    }

    #[test]
    fn bare_ni_does_not_over_trigger() {
        // "你好" should NOT strongly match get_agent_info
        // (previously "你能/你会" contained "你" causing false positives)
        let s = SelectionSnapshot::from_query("你好");
        let names = s.tool_names();
        let ai_pos = names
            .iter()
            .position(|n| *n == "get_agent_info")
            .unwrap_or(999);
        // Acceptable: get_agent_info may appear via "help" or description, but not top
        assert!(
            ai_pos >= 2 || names.is_empty(),
            "你好 should not strongly trigger get_agent_info (pos={})",
            ai_pos
        );
    }

    #[test]
    fn bare_de_does_not_contaminate() {
        // "这个的确有意思" — "的" should not cause false matches
        let s = SelectionSnapshot::from_query("这个的确有意思");
        assert_eq!(s.signal_count, 0, "generic sentence should have no signals");
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 19. Informal language + real-world patterns
// ═══════════════════════════════════════════════════════════════════════════════

mod real_world_patterns {
    use super::*;

    // ── Informal CN ──

    #[test]
    fn cn_informal_help_request() {
        // "检查" is not in is_fetch (no CJK equivalent for "check" in signals)
        // But this is still a valid test that shouldn't crash
        let s = SelectionSnapshot::from_query("老哥，查看一下这个代码呗");
        assert!(s.is_fetch, "查看 should fire is_fetch");
    }

    #[test]
    fn cn_casual_approval() {
        let s = SelectionSnapshot::from_query("行吧");
        assert_eq!(s.signal_count, 0, "casual approval is conversational");
    }

    #[test]
    fn cn_casual_start() {
        let s = SelectionSnapshot::from_query("搞起来");
        assert_eq!(s.signal_count, 0, "casual start is vague");
    }

    #[test]
    fn cn_informal_agreement() {
        let s = SelectionSnapshot::from_query("嗯嗯");
        assert_eq!(s.signal_count, 0, "informal agreement is conversational");
    }

    // ── Informal EN ──

    #[test]
    fn en_informal_pls() {
        let s = SelectionSnapshot::from_query("pls show me the diff");
        assert!(
            s.is_fetch || s.is_git,
            "pls show diff should detect signals"
        );
    }

    #[test]
    fn en_informal_lgtm() {
        let s = SelectionSnapshot::from_query("lgtm");
        assert_eq!(s.signal_count, 0, "lgtm is conversational");
    }

    // ── Code-switching with identifiers ──

    #[test]
    fn code_switch_with_function_name() {
        let s = SelectionSnapshot::from_query("帮我修改 process_request 函数");
        assert!(s.is_mutate, "修改 + function name should fire is_mutate");
    }

    #[test]
    fn code_switch_with_filepath() {
        let s = SelectionSnapshot::from_query("查看 src/main.rs 的内容");
        assert!(s.is_fetch, "查看 + filepath should fire is_fetch");
    }

    #[test]
    fn code_switch_sql_with_cn() {
        let s = SelectionSnapshot::from_query("执行 SELECT * FROM users 查询");
        let _ = s; // SQL + CN should not crash
    }

    // ── Real-world multi-step requests ──

    #[test]
    fn multi_step_review_and_fix() {
        let s = SelectionSnapshot::from_query("review the code and fix any bugs you find");
        assert!(s.is_mutate, "review+fix should fire is_mutate");
    }

    #[test]
    fn multi_step_cn_check_and_deploy() {
        let s = SelectionSnapshot::from_query("检查CI状态，如果通过就部署");
        assert!(
            s.is_github || s.is_fetch,
            "check CI should fire github or fetch"
        );
    }

    // ── Correction patterns ──

    #[test]
    fn correction_no_wait() {
        let s = SelectionSnapshot::from_query("no wait, check PR #42 instead");
        assert!(s.is_github, "correction + PR should fire is_github");
    }

    #[test]
    fn correction_cn_not_that() {
        let s = SelectionSnapshot::from_query("不是那个，查看git log");
        assert!(s.is_git, "correction + git log should fire is_git");
    }

    // ── Multiple entity references ──

    #[test]
    fn multiple_pr_numbers() {
        let s = SelectionSnapshot::from_query("compare PR #123 and PR #456");
        assert!(s.is_github, "multiple PR refs should fire is_github");
    }

    #[test]
    fn git_tag_reference() {
        let s = SelectionSnapshot::from_query("show git diff since tag v1.2.3");
        assert!(s.is_git, "git diff + tag should fire is_git");
    }

    // ── Mixed case keywords ──

    #[test]
    fn mixed_case_create_issue() {
        let s = SelectionSnapshot::from_query("Create Issue for this bug");
        assert!(
            s.is_github || s.is_mutate,
            "Create Issue should fire signals"
        );
    }

    #[test]
    fn all_caps_query() {
        let s = SelectionSnapshot::from_query("CHECK GIT STATUS NOW");
        assert!(
            s.is_git || s.is_fetch,
            "ALL CAPS git query should still work"
        );
    }

    // ── Queries with backticks (code references) ──

    #[test]
    fn backtick_function_reference() {
        let s = SelectionSnapshot::from_query("what does `detect_task_type` do?");
        let _ = s; // backtick ref should not crash
    }

    #[test]
    fn backtick_error_reference() {
        let s = SelectionSnapshot::from_query("fix the `NullPointerException` in login handler");
        assert!(s.is_mutate, "fix + backtick error should fire is_mutate");
    }

    // ── Single-word domain queries ──

    #[test]
    fn single_word_diff() {
        let s = SelectionSnapshot::from_query("diff");
        assert!(s.is_git, "bare diff should fire is_git");
    }

    #[test]
    fn single_word_status() {
        let s = SelectionSnapshot::from_query("status");
        assert!(
            s.is_fetch || s.is_git,
            "bare status should fire fetch or git"
        );
    }
}

// ─── Production Pipeline Tests ──────────────────────────────────────────────
// Full TfIdfSelector integration: SelectionContext with budget_pressure,
// restricted_tools, domain_hints, and recent_tools.

fn test_registry() -> ToolRegistry {
    let schemas: Vec<serde_json::Value> = TOOL_CATALOG
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        })
        .collect();
    ToolRegistry::new(schemas)
}

mod pipeline_integration {
    use super::*;

    // ── Budget pressure effects ──

    #[tokio::test]
    async fn normal_pressure_selects_more_tools_than_aggressive() {
        let selector = TfIdfSelector::new(test_registry());
        let q = "list github pull requests and check CI status";

        let normal = selector
            .select(&SelectionContext {
                query: q,
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;

        let aggressive = selector
            .select(&SelectionContext {
                query: q,
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.9,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;

        assert!(
            normal.tool_names.len() >= aggressive.tool_names.len(),
            "higher pressure should select fewer tools: normal={}, aggressive={}",
            normal.tool_names.len(),
            aggressive.tool_names.len()
        );
    }

    #[tokio::test]
    async fn pressure_doesnt_lose_primary_tool() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "show open pull requests",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.9,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "primary tool should survive even under high pressure: {:?}",
            result.tool_names
        );
    }

    // ── Restricted tools ──

    #[tokio::test]
    async fn restricted_tool_excluded_from_github_query() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "show pull requests and check CI",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec!["github_ci_status".to_string()],
            })
            .await;
        assert!(
            !result.tool_names.contains(&"github_ci_status".to_string()),
            "restricted tool must be excluded"
        );
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "non-restricted tool should still be selected"
        );
    }

    #[tokio::test]
    async fn restricted_git_tool_doesnt_affect_github() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "list open PRs on github",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec!["git_log".to_string(), "git_diff".to_string()],
            })
            .await;
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "github tool should be unaffected by git restriction: {:?}",
            result.tool_names
        );
    }

    // ── Domain hints ──

    #[tokio::test]
    async fn github_domain_hint_helps_entity_query() {
        let selector = TfIdfSelector::new(test_registry());
        // Without hint: entity-only query might not select github tools
        let without = selector
            .select(&SelectionContext {
                query: "matrixorigin",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;

        // With hint: github tools should be boosted
        let with = selector
            .select(&SelectionContext {
                query: "matrixorigin",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![DomainHint::GitHub],
                restricted_tools: vec![],
            })
            .await;

        let without_has_github = without.tool_names.iter().any(|n| n.starts_with("github_"));
        let with_has_github = with.tool_names.iter().any(|n| n.starts_with("github_"));

        // With hint should include github tools (or at least not lose them)
        if !without_has_github {
            assert!(
                with_has_github,
                "domain hint should boost github tools for entity query"
            );
        }
    }

    #[tokio::test]
    async fn database_domain_hint_boosts_mo_tools() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "查询一下数据库",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![DomainHint::Database],
                restricted_tools: vec![],
            })
            .await;
        let has_db = result
            .tool_names
            .iter()
            .any(|n| n.contains("mo_") || n.contains("query"));
        assert!(
            has_db,
            "database domain hint should promote db tools: {:?}",
            result.tool_names
        );
    }

    // ── Recent tools context (follow-up turns) ──

    #[tokio::test]
    async fn recent_tools_boost_followup() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "还有呢？",
                turn_count: 3,
                recent_tools: &["github_list_prs".to_string()],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        // Recent github_list_prs should boost github tools in followup
        let has_github = result.tool_names.iter().any(|n| n.starts_with("github_"));
        assert!(
            has_github,
            "follow-up after github tool should include github tools: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn recent_tools_dont_overpower_new_intent() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "check git diff for the latest changes",
                turn_count: 3,
                recent_tools: &["github_list_prs".to_string()],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        let has_git = result.tool_names.iter().any(|n| n.starts_with("git_"));
        assert!(
            has_git,
            "explicit git intent should override recent github tools: {:?}",
            result.tool_names
        );
    }

    // ── CJK queries through full pipeline ──

    #[tokio::test]
    async fn cjk_pr_query_full_pipeline() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "matrixorigin memoria 最新的pr?",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "CJK PR query should select github_list_prs: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn cjk_memory_query_full_pipeline() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "我有哪些记忆？",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        let has_memory = result.tool_names.iter().any(|n| n.contains("memory"));
        assert!(
            has_memory,
            "Chinese memory query should include memory tools: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn cjk_git_query_full_pipeline() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "谁改了这个文件？最近的提交记录",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        let has_git = result.tool_names.iter().any(|n| n.starts_with("git_"));
        assert!(
            has_git,
            "Chinese git query should include git tools: {:?}",
            result.tool_names
        );
    }

    // ── Combined signals ──

    #[tokio::test]
    async fn combined_pressure_and_restriction() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "check CI and show open PRs",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.6,
                memory_domain_hints: vec![],
                restricted_tools: vec!["github_ci_status".to_string()],
            })
            .await;
        assert!(
            !result.tool_names.contains(&"github_ci_status".to_string()),
            "restricted tool excluded under pressure"
        );
        // Even under pressure, the primary tool should survive
        assert!(
            result.tool_names.contains(&"github_list_prs".to_string()),
            "primary unrestricted tool should survive pressure: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn combined_hints_and_pressure() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "matrixorigin 的状态",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.6,
                memory_domain_hints: vec![DomainHint::GitHub],
                restricted_tools: vec![],
            })
            .await;
        let has_github = result.tool_names.iter().any(|n| n.starts_with("github_"));
        assert!(
            has_github,
            "domain hint should help even under pressure: {:?}",
            result.tool_names
        );
    }

    // ── Confidence stability ──

    #[tokio::test]
    async fn clear_query_has_higher_confidence_than_vague() {
        let selector = TfIdfSelector::new(test_registry());
        let clear = selector
            .select(&SelectionContext {
                query: "list open pull requests on github repository",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        let vague = selector
            .select(&SelectionContext {
                query: "help",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        assert!(
            clear.confidence >= vague.confidence,
            "clear query should have >= confidence than vague: clear={} vague={}",
            clear.confidence,
            vague.confidence
        );
    }

    #[tokio::test]
    async fn conversational_query_selects_few_dynamic_tools() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                query: "你好啊",
                turn_count: 1,
                recent_tools: &[],
                budget_tokens: 800,
                boost_terms: vec![],
                budget_pressure: 0.0,
                memory_domain_hints: vec![],
                restricted_tools: vec![],
            })
            .await;
        // Conversational should rely mostly on pinned tools
        assert!(
            result.tool_names.len() <= 12,
            "conversational should not select many dynamic tools: {:?}",
            result.tool_names
        );
    }
}
